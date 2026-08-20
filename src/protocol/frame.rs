use serde::{Deserialize, Serialize};

use crate::delta::Delta;

/// Build fingerprint: an FNV-1a hash of every source file, computed by
/// `build.rs`. cp2 has no released v1, so the wire format is never locked to
/// a version — the only requirement is that both peers run the *same build*.
/// The Hello handshake and the auto-deploy compare this fingerprint instead
/// of a hand-maintained protocol number: any source change (format,
/// behavior, or performance) automatically forces a redeploy, and a stale
/// remote fails the handshake cleanly. Wire-format history is preserved
/// below for reference:
///
/// v19: chunked-stream frames travel as a custom zero-copy layout (bit 30 of
/// the length prefix: `[file_id][raw bytes]`, no postcard envelope).
/// v18: `FileMeta` carries `atime`/`atime_nsec` and `xattrs` (`-U`/`-X`).
/// v17: archive fidelity — `uid`/`gid`/`mtime_nsec` in `FileMeta`, SUID/SGID/
/// Sticky on the wire.
pub const BUILD_FINGERPRINT: &str = env!("CP2_BUILD_FINGERPRINT");

/// Stable identifier for a file on the wire: a hash of its path, cheap to
/// derive identically on both sides. Chunk frames travel with the id alone
/// (no per-chunk path string).
pub type FileId = u64;

/// The operating system of a sync peer. The source side needs the *target*
/// OS to build the transfer list: the permission matrix (spec §2.2) and the
/// link representation (spec §3.2) both depend on it, and the receiver never
/// re-decides (spec §0 预决策). On a push the client knows it from the
/// platform probe; on a pull the client reports its own OS in the
/// `PullRequest` so the server-side sender can decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetOs {
    /// Unix-like (Linux, macOS, ...). POSIX permission bits and symlinks
    /// exist; `chmod`/`symlink` apply.
    Unix,
    /// Windows. Permission bits are not applied (NTFS ACLs are inherited)
    /// and links are represented as `.lnk` shortcuts or copied content.
    Windows,
}

impl TargetOs {
    /// Classify a probed OS name ("linux", "windows", "darwin", ...): anything
    /// containing "windows" is Windows; everything else is treated as
    /// Unix-like (the values the probe produces for the supported platforms).
    #[must_use]
    pub fn from_os_name(os: &str) -> Self {
        if os.to_ascii_lowercase().contains("windows") {
            TargetOs::Windows
        } else {
            TargetOs::Unix
        }
    }
}

/// The `--version` banner: crate version plus the build fingerprint.
///
/// Auto-deploy compares the fingerprint to decide whether the remote binary
/// is stale (any source change alters it — see [`BUILD_FINGERPRINT`]), so a
/// mismatched remote is redeployed instead of failing the handshake. The
/// banner is kept in lockstep with the constant by
/// [`tests::banner_matches_fingerprint`].
pub const VERSION_BANNER: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (build ",
    env!("CP2_BUILD_FINGERPRINT"),
    ")"
);

/// Protocol frame types.
///
/// Frame layout adapted from sparsync protocol.rs: a tagged union exchanged
/// over a framed byte stream, with a Hello handshake for version/feature
/// negotiation before any data flows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    /// Handshake: client announces its build fingerprint.
    Hello {
        /// Build fingerprint (see [`BUILD_FINGERPRINT`]).
        fingerprint: String,
    },
    /// Handshake response: server accepts or rejects.
    HelloAck {
        /// The server's build fingerprint.
        fingerprint: String,
        /// Whether the handshake was accepted.
        accepted: bool,
    },

    /// Sender announces its file manifest.
    ///
    /// `path` is the push target on the server, relative to its serve root
    /// (the account home by default): `cp2 ./data user@host/backup` sends
    /// `path = "/backup"` and the receiver applies into `root/backup`.
    IndexRequest {
        file_list: Vec<FileMeta>,
        /// Target directory on the receiver, relative to its serve root.
        path: String,
        /// Ask the receiver to return a per-file BLAKE3 hash of everything it
        /// applies (set when the sender runs `--remove-source-files`, so it
        /// can verify the destination bytes before deleting the source).
        verify: bool,
    },
    /// Receiver responds with its destination manifest.
    IndexResponse { file_list: Vec<FileMeta> },

    /// The sender asks the receiver for block signatures of specific files it
    /// plans to delta-transfer against (see [`Frame::SignatureResponse`]).
    ///
    /// Signatures are requested on demand, only for files the planner actually
    /// needs to delta — never for the whole tree.
    SignatureRequest {
        /// Relative paths whose destination signatures are needed.
        paths: Vec<String>,
    },
    /// The receiver's signatures for a [`Frame::SignatureRequest`].
    SignatureResponse {
        /// Path → signature for every requested path (the receiver signs files
        /// it actually has; missing files are omitted).
        signatures: Vec<SignatureEntry>,
    },

    /// Pull request (rsync-style `cp2 host:path local`).
    ///
    /// The client asks the server to send the directory at `path`; the server
    /// then plays the "sender" role (`IndexRequest` → `IndexResponse` → recipes).
    PullRequest {
        /// Remote path to send (must be under the server's root).
        path: String,
        /// Client-side exclude globs, applied by the server to its source scan.
        excludes: Vec<String>,
        /// Client-side include globs, overriding `excludes`.
        includes: Vec<String>,
        /// Client-side decision flags, applied by the server's planner.
        checksum: bool,
        delete: bool,
        update_only: bool,
        ignore_existing: bool,
        /// rsync `--existing`: only update files present on the receiver, do
        /// not create new ones (directories are still created).
        existing: bool,
        /// rsync `--ignore-times`: transfer everything, ignoring the
        /// size+mtime quick check.
        ignore_times: bool,
        /// Watch mode (`-W`): the server watches its own source tree and
        /// drives incremental pull cycles over this persistent session until
        /// the client disconnects. `watch_delay_ms` is the server-side
        /// debounce window.
        watch: bool,
        /// Debounce quiet window for watch mode, in milliseconds.
        watch_delay_ms: u32,
        /// Client-side `-z` and `--bwlimit` (bytes/s), applied by the server
        /// when it plays the sender.
        compress: bool,
        bwlimit: Option<u64>,
        /// The client's own OS, reported so the server-side sender (pull)
        /// can build the permission matrix and link representation for the
        /// correct target (spec §2.2 / §3.2 — the source side decides).
        client_os: TargetOs,
    },

    /// A delta recipe for one file. The delta carries literal bytes inline,
    /// so no separate data stream is needed.
    DeltaRecipe {
        /// The file this recipe applies to (see [`FileId`]).
        file_id: FileId,
        /// Relative path of the file.
        file_path: String,
        /// The delta (Copy ops reference the basis; Literal ops carry data).
        delta: Delta,
        /// The sender's chunk signature of the source — exactly the basis
        /// signature the new destination content will have. The receiver
        /// caches it (keyed by the applied file's size+mtime) so the next
        /// run's basis signing can skip re-reading the file. `None` when the
        /// sender did not chunk the source (whole-literal paths).
        source_signature: Option<crate::delta::Signature>,
        /// Cross-file basis: the delta's Copy ops reference this *other*
        /// file (relative path, already applied this run or present on the
        /// destination) instead of the file itself. `None` for the normal
        /// same-file basis.
        basis_path: Option<String>,
    },

    /// Multiple small-file deltas batched into a single frame.
    ///
    /// Reduces per-frame overhead for many tiny files.
    Batch {
        /// The batched recipes.
        recipes: Vec<BatchFile>,
    },

    /// Announce a large new file streamed as sequential [`Frame::FileChunk`]
    /// frames (rsync-style), terminated by [`Frame::FileEnd`]. Used instead of
    /// a single whole-file [`Frame::DeltaRecipe`] so memory stays bounded and
    /// an interrupted transfer leaves a resumable partial at the destination.
    FileStart {
        /// The file being streamed (see [`FileId`]).
        file_id: FileId,
        /// Relative path of the file.
        file_path: String,
        /// Size of the file in bytes.
        size: u64,
    },
    /// A sequential slice of a file announced with [`Frame::FileStart`].
    /// Identified by `file_id` alone — the receiver tracks the in-flight file,
    /// so no per-chunk path string travels on the wire.
    FileChunk {
        /// The file this chunk belongs to.
        file_id: FileId,
        /// Raw bytes, appended in order.
        data: Vec<u8>,
    },
    /// Terminates a [`Frame::FileStart`] transfer; the receiver commits.
    FileEnd {
        /// The file being finalized.
        file_id: FileId,
    },

    /// Request to create directories (empty directories in the source tree).
    MakeDir {
        /// Relative paths of directories to create.
        paths: Vec<String>,
    },

    /// Request to create symbolic and hard links on the receiver.
    ///
    /// Sent by the sender after all file content has been transferred (hard
    /// link targets must already exist). A symlink's target is the rewritten,
    /// DEST-relative target string computed at scan time; a `.lnk` entry's
    /// target is likewise DEST-relative (backslash-separated when the
    /// receiver is Windows); a hard link's target is a root-relative path to
    /// the representative file of the same inode group.
    CreateLinks {
        /// Symbolic-link and `.lnk` entries (the kind says which).
        links: Vec<LinkSpec>,
        /// Hard links to create (each points at the representative file).
        hardlinks: Vec<HardlinkSpec>,
        /// Special files (fifos, sockets, devices) — contentless, created
        /// after all file content (rsync -a's `-D`; Unix-only, only when
        /// `--archive`).
        specials: Vec<SpecialSpec>,
    },

    /// Request to delete destination files and empty directories.
    DeleteRequest {
        /// Relative paths to remove.
        paths: Vec<String>,
    },

    /// Sender signals completion.
    Done {
        /// Number of files transferred.
        files: u64,
        /// Total bytes transferred.
        bytes: u64,
    },

    /// Receiver acknowledges completion, letting the sender close cleanly.
    Ack {
        /// Number of files received.
        files: u64,
        /// Total bytes received.
        bytes: u64,
        /// Files skipped instead of applied (locked, path too long, ...).
        skipped: Vec<SkippedFile>,
        /// Per-file BLAKE3 hashes of the applied files (wire path → hash),
        /// populated when the sender requested verification (`verify: true`).
        hashes: Vec<(String, [u8; 32])>,
    },

    /// Fatal protocol error.
    Error {
        /// Human-readable error message.
        message: String,
    },
}

/// A file the receiver skipped instead of applying.
///
/// Per-file conditions (another process holding the file open, a path longer
/// than `MAX_PATH`, a reserved device name, ...) no longer abort the whole
/// sync: the file is skipped, warned about in place, and reported to the
/// peer through [`Frame::Ack`] so the CLI can list it at the end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    /// Destination path of the skipped file (for display).
    pub path: String,
    /// Human-readable reason.
    pub reason: String,
}

impl SkippedFile {
    /// A skipped-file record for the summary.
    #[must_use]
    pub fn new(path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

/// A signature of an existing destination file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureEntry {
    /// Relative path of the file.
    pub file_path: String,
    /// Block signature used for delta computation.
    pub signature: crate::delta::Signature,
}

/// One file within a [`Frame::Batch`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchFile {
    /// The file this recipe applies to (see [`FileId`]).
    pub file_id: FileId,
    /// Relative path of the file.
    pub file_path: String,
    /// The delta (Copy ops reference the basis; Literal ops carry data).
    pub delta: Delta,
}

/// One small file's raw payload for the zero-copy batch frame (the default
/// path — the data goes straight to the wire, no postcard envelope; see
/// `protocol::stream::send_batch_raw`). The `-z` path serializes the same
/// content as [`BatchFile`] deltas instead (compression needs the full
/// payload anyway).
#[derive(Debug)]
pub struct BatchItem {
    /// The file this item applies to (see [`FileId`]).
    pub file_id: FileId,
    /// Relative path of the file.
    pub file_path: String,
    /// The file's full content.
    pub data: Vec<u8>,
    /// The sender's whole-file BLAKE3, when the post-transfer comparison
    /// (`--verify`/`--remove-source-files`) will consume it.
    pub checksum: Option<[u8; 32]>,
}

/// A hard link to create: `path` links to `target` (both wire-relative —
/// the target is the representative file of the same inode group).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardlinkSpec {
    /// Wire-relative path of the link.
    pub path: String,
    /// Wire-relative path of the existing target file.
    pub target: String,
}

/// A special file (fifo/socket/device) to create — contentless, Unix-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialSpec {
    /// Wire-relative path.
    pub path: String,
    /// The kind (`Fifo`, `Socket`, `BlockDevice`, or `CharDevice`).
    pub kind: FileKind,
    /// Device numbers for block/char devices (`rdev`); `None` otherwise.
    pub rdev: Option<u64>,
}

/// What kind of filesystem object an entry is. Regular files carry content;
/// everything else is contentless and travels like a symlink. `Fifo`,
/// `Socket`, `BlockDevice`, and `CharDevice` exist only on Unix-like systems
/// and are never produced by the scanner on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    /// Regular file (content transferred).
    File,
    /// Directory (only empty directories appear as entries).
    Dir,
    /// Symbolic link (target in `link_target`).
    Symlink,
    /// Named pipe (fifo).
    Fifo,
    /// Unix domain socket.
    Socket,
    /// Block device (`rdev` identifies it).
    BlockDevice,
    /// Character device (`rdev` identifies it).
    CharDevice,
}

/// How a contentless link entry is materialized on the receiver. Decided at
/// scan time on the source side (spec §3.2): a Unix target gets a real
/// symlink, a Windows target gets a `.lnk` shortcut, and external targets
/// never reach this frame (they are dereferenced or skipped before).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkKind {
    /// A POSIX symbolic link (`std::os::unix::fs::symlink`).
    Symlink,
    /// A Windows `.lnk` shortcut (Shell Link binary).
    Lnk,
}

/// One link entry within a [`Frame::CreateLinks`] frame: the relative path
/// to create and its (already rewritten) target, plus the kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkSpec {
    /// Relative path of the link on the receiver.
    pub path: String,
    /// Target of the link: DEST-relative for symlinks/`.lnk` entries,
    /// root-relative for hard links (hard links stay in `hardlinks`).
    pub target: String,
    /// Whether the receiver creates a symlink or a `.lnk` shortcut.
    pub kind: LinkKind,
}

/// File metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub path: String,
    pub size: u64,
    pub mtime: u64,
    /// Nanosecond remainder of the mtime (whole-second wire `mtime` +
    /// `mtime_nsec`). Always carried and restored at apply time — the quick
    /// check compares it unconditionally.
    pub mtime_nsec: u32,
    pub mode: u32,
    /// BLAKE3 hash for content verification.
    pub hash: Option<[u8; 32]>,
    /// What kind of filesystem object this is.
    pub kind: FileKind,
    /// Link target; `Some` for a symbolic link (rewritten to a DEST-relative
    /// path at scan time, spec §3.2) or a `.lnk` shortcut's target.
    ///
    /// The link *kind* is deliberately not carried on the wire: the source
    /// alone decides how a link is materialized (see [`LinkKind`]), and the
    /// receiver executes the `LinkSpec.kind` the sender chose.
    pub link_target: Option<String>,
    /// Source inode (Unix); `None` on platforms without inodes (Windows) —
    /// hard links are only preserved when both sides carry inodes.
    pub inode: Option<u64>,
    /// Device number for block/char devices (Unix); `None` otherwise.
    pub rdev: Option<u64>,
    /// Source owner uid (Unix); `None` on Windows. Restored by `-a` with a
    /// best-effort `chown` (EPERM as a non-root receiver warns and keeps the
    /// SSH user's ownership — the default 0-Root model).
    pub uid: Option<u32>,
    /// Source owner gid (Unix); `None` on Windows. See `uid`.
    pub gid: Option<u32>,
    /// Source last-access time in whole seconds — always carried (cheap), the
    /// receiver applies it only under `--atimes` (`-U`), leaving atime alone
    /// otherwise (`UTIME_OMIT`). Not part of the quick check.
    pub atime: u64,
    /// Nanosecond remainder of the atime. See `atime`.
    pub atime_nsec: u32,
    /// Extended attributes (`--xattrs`, `-X`): name/value pairs for files and
    /// directories. `None` when the feature is off (nothing on the wire);
    /// `Some` (possibly empty) when enabled. Symlinks are not covered.
    pub xattrs: Option<Vec<(String, Vec<u8>)>>,
}

impl FileMeta {
    // Field-by-field constructor mirroring the struct's wire fields.
    #[expect(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        path: String,
        size: u64,
        mtime: u64,
        mtime_nsec: u32,
        mode: u32,
        hash: Option<[u8; 32]>,
        kind: FileKind,
        link_target: Option<String>,
        inode: Option<u64>,
        rdev: Option<u64>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: u64,
        atime_nsec: u32,
        xattrs: Option<Vec<(String, Vec<u8>)>>,
    ) -> Self {
        FileMeta {
            path,
            size,
            mtime,
            mtime_nsec,
            mode,
            hash,
            kind,
            link_target,
            inode,
            rdev,
            uid,
            gid,
            atime,
            atime_nsec,
            xattrs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_matches_fingerprint() {
        // The deploy banner is built from the same env the constant reads; a
        // mismatch here would silently break the build-aware stale check.
        assert!(
            VERSION_BANNER.contains(BUILD_FINGERPRINT),
            "banner {VERSION_BANNER:?} must carry the build fingerprint"
        );
    }

    #[test]
    fn fingerprint_is_hex() {
        assert_eq!(BUILD_FINGERPRINT.len(), 16);
        assert!(BUILD_FINGERPRINT
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn core_frames_roundtrip() {
        let frames = vec![
            Frame::Hello {
                fingerprint: BUILD_FINGERPRINT.to_string(),
            },
            Frame::HelloAck {
                fingerprint: BUILD_FINGERPRINT.to_string(),
                accepted: true,
            },
            Frame::IndexRequest {
                file_list: vec![FileMeta::new(
                    "a.txt".to_string(),
                    5,
                    0,
                    0,
                    0o644,
                    None,
                    FileKind::File,
                    None,
                    Some(42),
                    None,
                    Some(1000),
                    Some(1000),
                    1_600_000_000,
                    0,
                    None,
                )],
                path: "/".to_string(),
                verify: true,
            },
            Frame::IndexResponse {
                file_list: Vec::new(),
            },
            Frame::SignatureRequest {
                paths: vec!["a.bin".to_string()],
            },
            Frame::PullRequest {
                path: "/backup".to_string(),
                excludes: vec![],
                includes: vec![],
                checksum: false,
                delete: false,
                update_only: false,
                ignore_existing: false,
                existing: true,
                ignore_times: false,
                watch: true,
                watch_delay_ms: 250,
                compress: false,
                bwlimit: None,
                client_os: TargetOs::Unix,
            },
            Frame::CreateLinks {
                links: vec![LinkSpec {
                    path: "link.txt".to_string(),
                    target: "target.txt".to_string(),
                    kind: LinkKind::Symlink,
                }],
                hardlinks: vec![HardlinkSpec {
                    path: "dup.txt".to_string(),
                    target: "orig.txt".to_string(),
                }],
                specials: vec![SpecialSpec {
                    path: "pipe".to_string(),
                    kind: FileKind::Fifo,
                    rdev: None,
                }],
            },
            Frame::Ack {
                files: 1,
                bytes: 100,
                skipped: vec![SkippedFile {
                    path: "locked.bin".into(),
                    reason: "another process holds it open".into(),
                }],
                hashes: vec![("a.txt".to_string(), [0u8; 32])],
            },
        ];
        for frame in &frames {
            let bytes = postcard::to_allocvec(frame).unwrap();
            let decoded: Frame = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(format!("{frame:?}"), format!("{decoded:?}"));
        }
    }

    #[test]
    fn signature_request_response_roundtrip() {
        let request = Frame::SignatureRequest {
            paths: vec!["a.bin".to_string(), "sub/c.bin".to_string()],
        };
        let bytes = postcard::to_allocvec(&request).unwrap();
        let decoded: Frame = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(format!("{request:?}"), format!("{decoded:?}"));

        let response = Frame::SignatureResponse {
            signatures: vec![SignatureEntry {
                file_path: "a.bin".to_string(),
                signature: crate::delta::Signature::new(1234),
            }],
        };
        let bytes = postcard::to_allocvec(&response).unwrap();
        let decoded: Frame = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(format!("{response:?}"), format!("{decoded:?}"));
    }
}
