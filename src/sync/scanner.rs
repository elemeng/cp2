//! Directory scanner producing a serializable [`Manifest`].
//!
//! Adapted from the sparsync scan.rs pipeline: file discovery is a separate
//! phase from data transfer. The scanner walks the filesystem and returns a
//! [`Manifest`]; the transfer layer never walks the tree itself.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use soft_canonicalize::soft_canonicalize;
use typed_path::Utf8UnixPathBuf;

use crate::Result;
use crate::protocol::{FileKind, LinkKind, TargetOs};
use crate::sync::filter::FilterSet;
use crate::sync::linkpolicy::{
    LinkClass, classify_link, compute_exec_hint, final_mode, rewrite_internal_target,
};
use crate::sync::wire::{file_source, wire_str};

/// A single file (or empty directory) discovered by the scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path relative to the scan root.
    pub relative_path: String,
    /// File size in bytes (0 for directories and links).
    pub size: u64,
    /// POSIX permission bits — the *final* wire value on a source scan (spec
    /// §2.2 matrix), the raw bits on a destination scan (mode is unused
    /// there).
    pub mode: u32,
    /// Modification time in seconds since the Unix epoch.
    pub mtime_sec: i64,
    /// Nanosecond remainder of `mtime_sec` — always recorded, carried on the
    /// wire, and restored at apply time (the quick check compares it
    /// unconditionally).
    pub mtime_nsec: u32,
    /// BLAKE3 hash of the file contents (present when hashing is enabled).
    pub file_hash: Option<[u8; 32]>,
    /// Whether this entry is a directory. Only *empty* directories appear as
    /// entries; non-empty ones are implied by their files.
    pub is_dir: bool,
    /// Link target; `Some` for a symbolic link or `.lnk` shortcut — rewritten
    /// to a DEST-relative target at scan time (spec §3.2), `None` for regular
    /// files and directories.
    pub link_target: Option<String>,
    /// How a link entry is materialized (symlink vs `.lnk`); irrelevant for
    /// content entries.
    pub link_kind: LinkKind,
    /// Source inode (Unix); `None` on platforms without inodes — hard links
    /// are only preserved when both sides carry inodes.
    pub inode: Option<u64>,
    /// What kind of filesystem object this is (regular files carry content;
    /// dirs, links, and specials are contentless).
    pub kind: FileKind,
    /// Device number for block/char devices (Unix); `None` otherwise.
    pub rdev: Option<u64>,
    /// Source owner uid (Unix); `None` on platforms without ownership
    /// (Windows). Restored by `-a` with a best-effort `chown` (a non-root
    /// receiver keeps the SSH user's ownership — the default 0-Root model).
    pub uid: Option<u32>,
    /// Source owner gid (Unix); `None` on Windows. See `uid`.
    pub gid: Option<u32>,
    /// Last-access time in seconds since the Unix epoch — always recorded,
    /// carried on the wire; the receiver applies it only under `--atimes`
    /// (`UTIME_OMIT` otherwise). Not part of the quick check.
    pub atime_sec: i64,
    /// Nanosecond remainder of `atime_sec`. See `atime_sec`.
    pub atime_nsec: u32,
    /// Extended attributes (`--xattrs`): name/value pairs for files and
    /// directories; `None` when the feature is off, `Some` (possibly empty)
    /// when enabled. Symlinks are not covered.
    pub xattrs: Option<Vec<(String, Vec<u8>)>>,
    /// On-disk source of the content when it lives outside the scan root
    /// (`--follow-links` recursion into a directory referent). `None` means
    /// the content resolves as `root.join(relative_path)`.
    pub source_path: Option<PathBuf>,
    /// Whether the entry's content origin is outside the scan root. Such
    /// entries are never deleted by `--remove-source-files` (the source path
    /// is not ours to remove).
    pub dereferenced: bool,
}

/// The result of scanning a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Canonical scan root.
    pub root: PathBuf,
    /// All discovered files (sorted by relative path).
    pub files: Vec<FileEntry>,
    /// Sum of all file sizes.
    pub total_bytes: u64,
    /// Relative paths the scan *skipped* by policy (external directory links
    /// without `--literal-external-dir-links`, dangling external links,
    /// `--skip-links`). The sender protects these from `--delete`: they
    /// are not in `files`, so without this list a `--delete` run would treat
    /// a previously-created destination link as an extra and remove it.
    pub skipped: Vec<String>,
}

impl Manifest {
    /// Create an empty manifest.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            files: Vec::new(),
            total_bytes: 0,
            skipped: Vec::new(),
        }
    }

    /// Number of files in the manifest.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the manifest has no files.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

}

/// Scanner configuration.
#[derive(Debug, Clone)]
// Each boolean mirrors an independent rsync-style decision flag; grouping
// them would obscure the one-to-one mapping (see `cli.rs`).
#[expect(clippy::struct_excessive_bools)]
pub struct ScanOptions {
    /// Compute BLAKE3 hashes of every file.
    pub hash: bool,
    /// Number of parallel hashing workers (unused when `hash` is false).
    pub hash_workers: usize,
    /// Optional include/exclude filter applied to discovered paths.
    pub filter: Option<FilterSet>,
    /// Sync recursively (rsync `-r`); off means only the root's direct
    /// files are scanned and subdirectories are skipped.
    pub recursive: bool,
    /// Preserve symlinks as links (rsync `-l`); off (`--skip-links`) means
    /// every symlink and shortcut is skipped entirely — not synced, not
    /// followed. Highest priority over the other link knobs.
    pub preserve_links: bool,
    /// Dereference every symlink (`--follow-links`, rsync `-L`): the target's
    /// content is scanned in the link's place — file targets as regular
    /// files, directory targets recursed with loop detection. Overrides the
    /// `literal_*` knobs.
    pub follow_links: bool,
    /// Keep every link as a link with its literal target string
    /// (`--literal-links`, implied by `-a`): no DEST-relative rewrite, no
    /// external-link dereference or skip; a Windows-source `.lnk` is copied
    /// as an opaque file rather than interpreted.
    pub literal_links: bool,
    /// Keep *internal* links with their literal target string instead of the
    /// DEST-relative rewrite (`--literal-internal-links`); the external-link
    /// policy is unchanged.
    pub literal_internal_links: bool,
    /// Keep external *file-target* links as links with their literal target
    /// instead of dereferencing them (`--literal-external-file-links`).
    /// Ignored when the target is Windows (a Windows receiver cannot
    /// represent a POSIX absolute link).
    pub literal_external_file_links: bool,
    /// Keep external *directory-target* links as links with their literal
    /// target instead of skipping them (`--literal-external-dir-links`).
    pub literal_external_dir_links: bool,
    /// The target side's OS — drives the permission matrix and link
    /// representation (spec §2.2 / §3.2). The source side decides.
    pub target_os: TargetOs,
    /// Whether this scan is the *source* manifest: the permission matrix is
    /// applied only then (destination modes have no consumer).
    pub is_source_scan: bool,
    /// `--no-perms`: the matrix yields explicit 0644/0755 defaults instead
    /// of source-derived bits (and disables the Windows `exec_hint`).
    pub no_perms: bool,
    /// The OS this scanner's *source* entries come from. Normally the local OS
    /// ([`local_os`]); tests override it to exercise the Windows-source `.lnk`
    /// and `exec_hint` paths on any host. Drives `.lnk` recognition and the
    /// §2.2 permission matrix.
    pub source_os: TargetOs,
    /// `-a` (archive): the §2.2 matrix keeps SUID/SGID/Sticky (`& 0o7777`,
    /// byte-identical mode) instead of the default `& 0o777` clearing.
    pub archive: bool,
    /// `-X` (`--xattrs`): collect extended attributes for files and
    /// directories (best-effort; symlinks are not covered) and carry them on
    /// the wire. Off by default — collection is per-entry syscalls.
    pub xattrs: bool,
    /// rsync trailing-slash semantics: when the source root is a directory and
    /// the source path had no trailing slash, the directory's own name is
    /// recreated at the destination (`cp2 dir DST` → `DST/dir/*`). When set,
    /// the scanner prefixes every entry's relative path with the root's file
    /// name and roots the manifest at the *parent* directory, so the sender
    /// reads `parent/name/...` and the receiver builds `DST/name/...`. A
    /// trailing slash on the source (contents mode) leaves this `false`.
    /// Source scans only — never set on a destination scan.
    pub include_root_component: bool,
    /// Relative paths the *peer* source manifest classifies as links (symlinks
    /// or `.lnk` shortcuts). Set only on a destination scan: it lets the walk
    /// recognize a materialized shortcut by content alone — a Unix-source
    /// symlink becomes an *extensionless* `.lnk` on a Windows target, which
    /// the `.lnk` extension gate would miss — without misclassifying an
    /// arbitrary data file whose body merely starts with the `.lnk` magic. The
    /// targeted probe derives this from the source manifest it is given
    /// (`Scanner::scan_targeted`); the full walk (`--delete`) needs it carried
    /// here.
    pub source_link_paths: Option<HashSet<String>>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            hash: false,
            hash_workers: num_cpus::get(),
            filter: None,
            recursive: true,
            preserve_links: true,
            follow_links: false,
            literal_links: false,
            literal_internal_links: false,
            literal_external_file_links: false,
            literal_external_dir_links: false,
            target_os: TargetOs::Unix,
            is_source_scan: true,
            no_perms: false,
            source_os: local_os(),
            source_link_paths: None,
            archive: false,
            xattrs: false,
            include_root_component: false,
        }
    }
}

/// rsync trailing-slash semantics for a source path *string*: whether the
/// source's last path component should be recreated at the destination
/// (`cp2 dir DST` → `DST/dir/*`). "Contents" mode (`false`) applies when the
/// path is empty, ends with a separator (`dir/`), or its last component is
/// `.` or `..`; otherwise the component is included (`true`).
#[must_use]
pub(crate) fn include_root_component(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    if path.ends_with(['/', '\\']) {
        return false;
    }
    match path.rsplit(['/', '\\']).next() {
        Some(comp) => !comp.is_empty() && comp != "." && comp != "..",
        None => false,
    }
}

/// The OS this process runs on: the default `source_os` for a scanner reading
/// the local filesystem (drives `.lnk` recognition and the §2.2 permission
/// matrix). Tests override `source_os` to simulate the peer side on any host.
#[must_use]
pub(crate) fn local_os() -> TargetOs {
    if cfg!(windows) {
        TargetOs::Windows
    } else {
        TargetOs::Unix
    }
}

/// Filesystem scanner.
#[derive(Debug, Clone)]
pub struct Scanner {
    options: ScanOptions,
}

impl Scanner {
    /// Create a new scanner.
    #[must_use]
    pub fn new(options: ScanOptions) -> Self {
        Self { options }
    }

    /// Scan a directory (or single file) into a [`Manifest`].
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the tree cannot be read.
    pub async fn scan(&self, root: &Path) -> Result<Manifest> {
        let canonical = tokio::fs::canonicalize(root)
            .await
            .map_err(crate::Error::Io)?;

        let mut entries = Vec::new();

        // Single-file scan: the root itself is the file.
        if canonical.is_file() {
            let rel = wire_str(
                &canonical
                    .file_name()
                    .unwrap_or(canonical.as_os_str())
                    .to_string_lossy(),
            );
            if self.options.filter.as_ref().is_none_or(|f| f.passes(&rel)) {
                let meta = tokio::fs::metadata(&canonical)
                    .await
                    .map_err(crate::Error::Io)?;
                entries.push(file_entry(rel, &meta, None));
            }
            if self.options.hash {
                // Entries are named by their file name; `hash_files` joins
                // against the scan root, so root here must be the *parent*
                // directory, not the file itself.
                let parent = canonical.parent().unwrap_or(&canonical);
                self.hash_files(parent, &mut entries).await?;
            }
            self.apply_permission_matrix(&mut entries);
            self.collect_entry_xattrs(
                canonical.parent().unwrap_or(&canonical),
                &mut entries,
            );
            let total_bytes = entries.iter().map(|f| f.size).sum();
            // Root at the *parent*: the entry is named by the file's name, so
            // the transfer layer resolves it as `parent/name` — rooting at the
            // file itself would double the name (`file/name` → ENOTDIR).
            return Ok(Manifest {
                root: canonical
                    .parent()
                    .unwrap_or(&canonical)
                    .to_path_buf(),
                files: entries,
                total_bytes,
                skipped: Vec::new(),
            });
        }

        // Walk the tree in parallel (jwalk / rayon), applying the filter to
        // prune excluded directories and files. The whole walk runs on a
        // blocking thread so it never stalls the async reactor.
        let ctx = self.context(&canonical);
        let root_for_walk = canonical.clone();
        let (mut entries, skipped): (Vec<FileEntry>, Vec<String>) =
            tokio::task::spawn_blocking(move || {
                walk_tree(&ctx, &root_for_walk)
            })
            .await
            .map_err(|e| crate::Error::Other(format!("Scan task panicked: {e}")))??;

        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

        if self.options.hash {
            self.hash_files(&canonical, &mut entries).await?;
        }
        self.apply_permission_matrix(&mut entries);
        self.collect_entry_xattrs(&canonical, &mut entries);

        // rsync trailing-slash semantics (`cp2 dir DST` → `DST/dir/*`): the
        // source dir's own name is recreated, so entries are named relative to
        // the *parent* and prefixed with the root component. Guard the
        // filesystem root — `/` has no file name to include. Hashing and the
        // permission/xattr passes above already ran against `canonical`, so
        // only the wire names and the manifest root change here.
        let root = if self.options.include_root_component
            && let Some(dir_name) = canonical.file_name()
        {
            // Re-home the entries under a `{name}/` prefix. Built through a
            // `Utf8UnixPathBuf` push so the '/'-join is Unix-semantic on every
            // host (the wire form is '/' regardless of the build target).
            let prefix = dir_name.to_string_lossy().into_owned();
            for entry in &mut entries {
                let mut wire = Utf8UnixPathBuf::new();
                wire.push(&prefix);
                wire.push(&entry.relative_path);
                entry.relative_path = wire.to_string();
            }
            canonical
                .parent()
                .unwrap_or(&canonical)
                .to_path_buf()
        } else {
            canonical.clone()
        };

        let total_bytes = entries.iter().map(|f| f.size).sum();
        Ok(Manifest {
            root,
            files: entries,
            total_bytes,
            skipped,
        })
    }

    /// Scan several roots into a single [`Manifest`] whose entries are
    /// relative to `base` (glob-expanded sources): each root becomes a
    /// top-level entry named by its path under `base`, so all matches sync as
    /// one plan. Matched directories are added as entries themselves — empty
    /// matches and their metadata are preserved — and the filter is applied to
    /// each match before its subtree is scanned, so `--exclude` drops whole
    /// matches.
    ///
    /// # Errors
    ///
    /// Returns an error if a match is not under `base` or a tree cannot be
    /// read.
    pub async fn scan_multi(&self, base: &Path, roots: &[PathBuf]) -> Result<Manifest> {
        let mut entries = Vec::new();
        let mut skipped = Vec::new();
        // Ancestors before descendants (lexical order), so a root already
        // covered by an earlier accepted root is skipped instead of scanned
        // twice — a `--files-from` list or a `**` glob can name both a
        // directory and files inside it, or the same path twice.
        let mut roots = roots.to_vec();
        roots.sort();
        let mut accepted: Vec<PathBuf> = Vec::new();
        // Link classification anchors against the canonical merge base —
        // canonicalized exactly as `scan` does: on macOS the base is often
        // reached through a symlinked prefix (`/var` → `/private/var`), and
        // a literal root would classify every internal link as external and
        // dereference it. An empty base (a "./" glob) means the cwd.
        let ctx_base = if base.as_os_str().is_empty() {
            Path::new(".")
        } else {
            base
        };
        let canonical_base = tokio::fs::canonicalize(ctx_base).await.map_err(crate::Error::Io)?;
        let ctx = self.context(&canonical_base);
        for root in &roots {
            // Equal to, or inside, an already-accepted root: already covered.
            if accepted.iter().any(|r| root.starts_with(r)) {
                continue;
            }
            let rel_os = root.strip_prefix(base).map_err(|_| {
                crate::Error::Other(format!(
                    "internal glob error: {} is not under {}",
                    root.display(),
                    base.display()
                ))
            })?;
            let rel = wire_str(&rel_os.to_string_lossy());
            // Glob matches come back as `./name` for a `./*` pattern; the
            // destination manifest names the same file `name`, so the leading
            // `./` must go or the planner would never quick-check-skip.
            let rel = rel.strip_prefix("./").unwrap_or(&rel).to_string();
            if self
                .options
                .filter
                .as_ref()
                .is_none_or(|f| f.passes(&rel))
            {
                self.scan_match(root, &rel, &ctx, &mut entries, &mut skipped)
                    .await?;
                accepted.push(root.clone());
            }
        }
        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        self.apply_permission_matrix(&mut entries);
        self.collect_entry_xattrs(base, &mut entries);
        let total_bytes = entries.iter().map(|f| f.size).sum();
        Ok(Manifest {
            root: base.to_path_buf(),
            files: entries,
            total_bytes,
            skipped,
        })
    }

    /// Scan one glob match `root` (its merged relative name is `rel`) into
    /// `entries`: a symlink becomes a link entry, a directory a dir entry plus
    /// its scanned subtree, a file a single entry — anything else (fifos,
    /// sockets, devices) is skipped, matching `walk_tree`. `ctx` anchors link
    /// classification against the merge base.
    async fn scan_match(
        &self,
        root: &Path,
        rel: &str,
        ctx: &ScanContext,
        entries: &mut Vec<FileEntry>,
        skipped: &mut Vec<String>,
    ) -> Result<()> {
        let lmeta = std::fs::symlink_metadata(root).map_err(crate::Error::Io)?;
        let file_type = lmeta.file_type();
        if file_type.is_symlink() {
            let target = std::fs::read_link(root)
                .map(|t| wire_str(&t.to_string_lossy()))
                .unwrap_or_default();
            let mut visited = HashSet::new();
            scan_link(ctx, root, rel, &target, Some(&lmeta), entries, skipped, &mut visited);
            return Ok(());
        }
        if !file_type.is_dir() && !file_type.is_file() {
            // Special file (fifo/socket/device): Unix-only; recorded when the
            // platform has such objects (Windows never produces them).
            if let Some(kind) = special_kind(&lmeta) {
                entries.push(special_entry(rel.to_string(), &lmeta, kind));
            }
            return Ok(());
        }
        if file_type.is_file() && ctx.source_is_windows && !ctx.literal_links && is_lnk_file(root) {
            // A Windows-source `.lnk` shortcut: classify it as a link (spec
            // §3.2) instead of copying its binary body — unless literal
            // preservation (`--literal-links`/`-a`) copies the body as-is.
            let mut visited = HashSet::new();
            scan_lnk(ctx, root, rel, &lmeta, entries, skipped, &mut visited);
            return Ok(());
        }

        let manifest = self.scan(root).await?;
        if file_type.is_dir() {
            // The matched directory itself, so empty matches and the
            // directory's metadata survive the merge.
            let meta = tokio::fs::metadata(root).await.map_err(crate::Error::Io)?;
            entries.push(dir_entry(rel.to_string(), &meta));
            for mut entry in manifest.files {
                entry.relative_path = wire_str(
                    &PathBuf::from(rel)
                        .join(&entry.relative_path)
                        .to_string_lossy(),
                );
                entries.push(entry);
            }
        } else if let Some(mut file) = manifest.files.into_iter().next() {
            // Single file: `scan` named it by its file name; rebase it.
            file.relative_path = rel.to_string();
            entries.push(file);
        }
        Ok(())
    }

    /// Scan the destination root for exactly the paths the peer's source
    /// manifest names — a source-keyed probe instead of a full walk, so a
    /// huge destination costs O(source), not O(destination).
    ///
    /// Every source path is `lstat`-ed against the root; a miss records
    /// nothing (an absent parent implies absent children, but probing them
    /// anyway costs one cheap failed stat each — the stats run concurrently,
    /// bounded by the hash-worker count, so the probe scales with the tree
    /// rather than serially). Type mismatches (a directory where the source
    /// has a file, and vice versa) are recorded as they are, so the planner
    /// schedules the replacement. No filter is applied — the destination is
    /// not filtered, only the transfer decision is (rsync semantics),
    /// matching the full walk.
    ///
    /// Callers use this when `--delete` is off. Deletion must name the
    /// extras it removes, which requires the full tree ([`Scanner::scan`]).
    ///
    /// # Errors
    ///
    /// Returns an I/O error if a probed path cannot be read (a missing path
    /// is not an error — it is simply absent).
    pub async fn scan_targeted(&self, root: &Path, source: &Manifest) -> Result<Manifest> {
        let canonical = tokio::fs::canonicalize(root)
            .await
            .map_err(crate::Error::Io)?;

        // Source paths sorted (owned — each stat task is `'static`).
        let mut paths: Vec<String> = source
            .files
            .iter()
            .map(|f| f.relative_path.clone())
            .collect();
        paths.sort_unstable();
        paths.dedup();

        // Destination probe: classify links the same way the source scan does
        // (rewritten targets must match for the quick check to agree), but
        // never apply the dereference policy, recurse into external
        // directories, or warn about skips — links are recorded *faithfully*
        // so a representation change in the source policy converges on the
        // next run, and a skipped external-dir link's children are probed by
        // their own source paths below.
        //
        // `.lnk` recognition is keyed on the source manifest: only paths the
        // source classifies as links are sniffed by content (a Unix-source
        // symlink materializes as an *extensionless* `.lnk` on a Windows
        // target), and a data file whose body merely starts with the `.lnk`
        // magic is never misclassified.
        let source_links: HashSet<String> = source
            .files
            .iter()
            .filter(|f| f.link_target.is_some())
            .map(|f| f.relative_path.clone())
            .collect();
        let ctx = std::sync::Arc::new(ScanContext {
            is_source: false,
            // The probe never dereferences (the `follow` branch records
            // faithfully on this side), but the literal knobs carry over:
            // under `--literal-*` the probe must record the same literal
            // targets as the source for the quick check to converge.
            follow_links: false,
            warn: false,
            source_link_paths: (!source_links.is_empty()).then_some(source_links),
            ..self.context(&canonical)
        });

        // Probe concurrently, the paths batched into one blocking task per
        // worker: the per-path async machinery (a spawn + a semaphore permit
        // + the tokio fs pool hop per stat) costs ~5x the stat itself —
        // batching amortizes it, exactly like the receiver's batch apply.
        // The probe never recurses, so a per-chunk visited set is equivalent
        // to one shared.
        let workers = self.options.hash_workers.max(1);
        let chunk_size = paths.len().div_ceil(workers).max(1);
        let mut handles = Vec::new();
        while !paths.is_empty() {
            let take = chunk_size.min(paths.len());
            let chunk: Vec<String> = paths.drain(..take).collect();
            let root = canonical.clone();
            let ctx = std::sync::Arc::clone(&ctx);
            handles.push(tokio::task::spawn_blocking(move || {
                let mut entries: Vec<FileEntry> = Vec::new();
                let mut visited = HashSet::new();
                for rel in chunk {
                    let full = root.join(&rel);
                    let result = std::fs::symlink_metadata(&full);
                    match result {
                        Ok(meta) => {
                            let file_type = meta.file_type();
                            if file_type.is_dir() {
                                entries.push(dir_entry(rel.clone(), &meta));
                            } else if file_type.is_file() {
                                if ctx.source_is_windows && is_lnk_entry(&full, &rel, &ctx) {
                                    scan_lnk(
                                        &ctx,
                                        &full,
                                        &rel,
                                        &meta,
                                        &mut entries,
                                        &mut Vec::new(),
                                        &mut visited,
                                    );
                                } else {
                                    entries.push(file_entry(rel.clone(), &meta, None));
                                }
                            } else if file_type.is_symlink() {
                                let target = std::fs::read_link(&full)
                                    .map(|t| wire_str(&t.to_string_lossy()))
                                    .unwrap_or_default();
                                scan_link(
                                    &ctx,
                                    &full,
                                    &rel,
                                    &target,
                                    Some(&meta),
                                    &mut entries,
                                    &mut Vec::new(),
                                    &mut visited,
                                );
                            } else if let Some(kind) = special_kind(&meta) {
                                // Special file (fifo/socket/device): Unix-only.
                                entries.push(special_entry(rel.clone(), &meta, kind));
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::NotFound
                                    | std::io::ErrorKind::NotADirectory
                            ) || is_symlink_loop(&e) =>
                        {
                            // The path cannot exist: missing, a parent is not
                            // a directory (a file replacing a directory, or
                            // vice versa), or a symlink loop in the parent
                            // chain (the full walk never follows links
                            // either). Absent — nothing recorded.
                        }
                        Err(e) => return Err(crate::Error::Io(e)),
                    }
                }
                Ok::<_, crate::Error>(entries)
            }));
        }

        let mut entries: Vec<FileEntry> = Vec::new();
        for handle in handles {
            entries.extend(
                handle
                    .await
                    .map_err(|_| crate::Error::Other("Probe task panicked".to_string()))??,
            );
        }

        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        if self.options.hash {
            self.hash_files(&canonical, &mut entries).await?;
        }
        let total_bytes = entries.iter().map(|f| f.size).sum();
        Ok(Manifest {
            root: canonical,
            files: entries,
            total_bytes,
            skipped: Vec::new(),
        })
    }

    /// Compute BLAKE3 hashes for all entries in parallel, streaming each file.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if any file cannot be read.
    async fn hash_files(&self, root: &Path, entries: &mut [FileEntry]) -> Result<()> {
        use tokio::io::AsyncReadExt;
        let workers = self.options.hash_workers.max(1);
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(workers));
        let root = std::sync::Arc::new(root.to_path_buf());
        let mut handles = Vec::new();

        for (idx, entry) in entries.iter().enumerate() {
            if entry.is_dir || entry.link_target.is_some() || entry.kind != FileKind::File {
                continue; // dirs, links, and specials carry no content hash
            }
            let permit = semaphore.clone().acquire_owned().await;
            let path = entry_source(&root, entry);
            handles.push(tokio::spawn(async move {
                let _permit = permit;
                let display = path.display().to_string();
                let mut file = tokio::fs::File::open(&path)
                    .await
                    .map_err(|e| crate::Error::Other(format!("Failed to hash {display}: {e}")))?;
                let mut hasher = blake3::Hasher::new();
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    let n = file.read(&mut buf).await.map_err(|e| {
                        crate::Error::Other(format!("Failed to hash {display}: {e}"))
                    })?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                }
                Ok::<_, crate::Error>((idx, *hasher.finalize().as_bytes()))
            }));
        }

        for handle in handles {
            let (idx, hash) = handle
                .await
                .map_err(|_| crate::Error::Other("Hash task panicked".to_string()))??;
            entries[idx].file_hash = Some(hash);
        }
        Ok(())
    }

    /// Build the scan context for the canonical `root`.
    fn context(&self, root: &Path) -> ScanContext {
        ScanContext {
            root: root.to_path_buf(),
            filter: self.options.filter.clone(),
            recursive: self.options.recursive,
            preserve_links: self.options.preserve_links,
            follow_links: self.options.follow_links,
            literal_links: self.options.literal_links,
            literal_internal_links: self.options.literal_internal_links,
            literal_external_file_links: self.options.literal_external_file_links,
            literal_external_dir_links: self.options.literal_external_dir_links,
            target_os: self.options.target_os,
            is_source: self.options.is_source_scan,
            source_is_windows: self.options.source_os == TargetOs::Windows,
            source_link_paths: self.options.source_link_paths.clone(),
            warn: self.options.is_source_scan,
        }
    }

    /// Apply the permission matrix (spec §2.2) to a *source* scan: the final
    /// wire mode replaces the raw bits. Destination scans keep the raw bits
    /// (nothing consumes them; the planner quick-checks size/mtime/hash only).
    fn apply_permission_matrix(&self, entries: &mut [FileEntry]) {
        if !self.options.is_source_scan {
            return;
        }
        let source_os = self.options.source_os;
        let no_perms = self.options.no_perms;
        let archive = self.options.archive;
        for entry in entries {
            // Links carry no mode; specials keep their raw bits (masked).
            if entry.link_target.is_some() {
                entry.mode = 0;
                continue;
            }
            let exec_hint = compute_exec_hint(Path::new(&entry.relative_path));
            entry.mode = final_mode(
                source_os,
                self.options.target_os,
                entry.mode,
                entry.is_dir,
                exec_hint,
                no_perms,
                archive,
            );
        }
    }

    /// Collect extended attributes (`-X`) for every file and directory entry
    /// — a post-pass because the entries are built from the on-disk paths,
    /// which [`entry_source`] resolves (dereferenced entries read from their
    /// explicit source). Best-effort: an unreadable attribute set yields an
    /// empty list rather than a scan failure, and symlinks are not covered
    /// (their attributes are link metadata, not content). Source scans only —
    /// a destination probe has no consumer for xattrs.
    fn collect_entry_xattrs(&self, root: &Path, entries: &mut [FileEntry]) {
        if !self.options.xattrs || !self.options.is_source_scan {
            return;
        }
        for entry in entries {
            if matches!(entry.kind, FileKind::File | FileKind::Dir) {
                let path = entry_source(root, entry);
                entry.xattrs = Some(crate::platform::fs::collect_xattrs(&path));
            }
        }
    }
}

/// Immutable context threaded through a scan: the policy knobs the link
/// classification and permission matrix need (spec §2/§3), plus the filter.
#[derive(Debug, Clone)]
#[expect(clippy::struct_excessive_bools)]
struct ScanContext {
    /// Canonical scan root (the internal/external containment anchor).
    root: PathBuf,
    filter: Option<FilterSet>,
    recursive: bool,
    preserve_links: bool,
    follow_links: bool,
    literal_links: bool,
    literal_internal_links: bool,
    literal_external_file_links: bool,
    literal_external_dir_links: bool,
    target_os: TargetOs,
    /// Whether this scan is the *source* side. The source applies the link
    /// policy (dereference/keep/skip); a destination probe records links
    /// *faithfully* (as links), so a representation change in the source
    /// policy always converges on the next run.
    is_source: bool,
    /// Whether the *source* runs on Windows (drives `.lnk` detection and the
    /// `exec_hint` heuristic).
    source_is_windows: bool,
    /// Relative paths the peer's source manifest classifies as links — the
    /// destination scan's `.lnk` recognition key (see [`ScanOptions`]).
    source_link_paths: Option<HashSet<String>>,
    /// Emit `tracing::warn` for policy skips (source scans only — a
    /// destination probe must not report the destination's links as
    /// "skipped").
    warn: bool,
}

impl ScanContext {
    /// Record a policy skip: warn (on source scans) and remember the wire
    /// path so the sender protects it from `--delete` (the path still exists
    /// in the source; it is simply not transferred).
    fn skip(&self, rel: &str, reason: &str, skipped: &mut Vec<String>) {
        if self.warn {
            tracing::warn!("skipping {rel}: {reason}");
        }
        skipped.push(rel.to_string());
    }
}

/// Walk `root` in parallel (jwalk / rayon), applying `filter` to prune
/// excluded directories and files, and collect an entry per file, directory,
/// and symlink, plus the wire paths of policy-skipped links. Wire paths are
/// always '/'-separated, on every OS.
fn walk_tree(ctx: &ScanContext, root: &Path) -> crate::Result<(Vec<FileEntry>, Vec<String>)> {
    let walk_root = root.to_path_buf();
    let filter_root = walk_root.clone();
    let filter = ctx.filter.clone();
    let recursive = ctx.recursive;
    let mut walker = jwalk::WalkDir::new(root)
        .skip_hidden(false) // dotfiles are synced
        .min_depth(1) // the root itself is not an entry
        .max_depth(if recursive { usize::MAX } else { 1 });
    walker = walker.process_read_dir(move |_parent, _dir_path, _state, dir_entries| {
            if let Some(filter) = &filter {
                dir_entries.retain(|e| match e {
                    Ok(entry) => {
                        let path = entry.path();
                        let rel = path.strip_prefix(&filter_root).unwrap_or(&path);
                        filter.passes(&rel.to_string_lossy())
                    }
                    Err(_) => true, // surface read errors below
                });
            }
        });

    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    for entry in walker {
        let entry = entry
            .map_err(|e| crate::Error::Other(format!("walk error: {e}")))?;
        let path = entry.path();
        let rel = path
            .strip_prefix(&walk_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let file_type = entry.file_type();
        if file_type.is_dir() {
            // `--no-recursive`: only the root's direct files; subdirectories
            // are skipped entirely.
            if !recursive {
                continue;
            }
            let meta = entry
                .metadata()
                .map_err(|e| crate::Error::Other(format!("metadata: {e}")))?;
            out.push(dir_entry(rel, &meta));
        } else if file_type.is_file() {
            if ctx.source_is_windows && is_lnk_entry(&path, &rel, ctx) {
                // A Windows-source `.lnk` shortcut: classified as a link
                // (spec §3.2), not copied as a plain binary.
                let lmeta = entry
                    .metadata()
                    .map_err(|e| crate::Error::Other(format!("metadata: {e}")))?;
                scan_lnk(ctx, &path, &rel, &lmeta, &mut out, &mut skipped, &mut visited);
            } else {
                let meta = entry
                    .metadata()
                    .map_err(|e| crate::Error::Other(format!("metadata: {e}")))?;
                out.push(file_entry(rel, &meta, None));
            }
        } else if file_type.is_symlink() {
            // jwalk does not follow links. The link's own (lstat) mtime is
            // kept so it can be restored with utimensat(AT_SYMLINK_NOFOLLOW).
            let target = std::fs::read_link(&path)
                .map(|t| wire_str(&t.to_string_lossy()))
                .unwrap_or_default();
            let lmeta = std::fs::symlink_metadata(&path).ok();
            scan_link(
                ctx,
                &path,
                &rel,
                &target,
                lmeta.as_ref(),
                &mut out,
                &mut skipped,
                &mut visited,
            );
        } else if let Some(lmeta) = std::fs::symlink_metadata(&path).ok()
            && let Some(kind) = special_kind(&lmeta)
        {
            // Special file (fifo/socket/device): Unix-only; recorded so
            // `--archive` can recreate it, ignored elsewhere.
            out.push(special_entry(rel, &lmeta, kind));
        }
    }
    Ok((out, skipped))
}

/// Decide what entries a link at `link_path` (relative path `rel`, literal
/// `target`) contributes, per the link policy (spec §3.2). This is the single
/// decision point shared by the full walk, the targeted destination probe,
/// and the `--follow-links` recursion.
#[expect(clippy::too_many_arguments)]
fn scan_link(
    ctx: &ScanContext,
    link_path: &Path,
    rel: &str,
    target: &str,
    lmeta: Option<&std::fs::Metadata>,
    entries: &mut Vec<FileEntry>,
    skipped: &mut Vec<String>,
    visited: &mut HashSet<PathBuf>,
) {
    // `--skip-links` wins over everything (spec §1): the link is not synced
    // and not followed — its target's content is never transferred. A
    // destination probe does not skip — it records the link faithfully, so a
    // representation change in the source policy converges on the next run.
    if !ctx.preserve_links {
        if ctx.is_source {
            ctx.skip(rel, "link skipped (--skip-links)", skipped);
        } else {
            entries.push(symlink_entry(
                rel.to_string(),
                target.to_string(),
                LinkKind::Symlink,
                lmeta,
            ));
        }
        return;
    }
    // `--follow-links` (rsync `-L`): dereference everything — the target's
    // content is copied in the link's place. File referents become regular
    // files; directory referents are recursed (with loop detection). A
    // destination probe records the link faithfully instead: the planner
    // sees the representation mismatch (the source sends a file) and
    // replaces the link.
    if ctx.follow_links {
        if ctx.is_source {
            match std::fs::metadata(link_path) {
                Ok(meta) if meta.is_file() => {
                    entries.push(file_entry(rel.to_string(), &meta, None));
                }
                Ok(meta) if meta.is_dir() => {
                    walk_external(ctx, link_path, rel, entries, skipped, visited);
                }
                _ => {
                    ctx.skip(rel, "unreadable or special link target (--follow-links)", skipped);
                }
            }
        } else {
            entries.push(symlink_entry(
                rel.to_string(),
                target.to_string(),
                LinkKind::Symlink,
                lmeta,
            ));
        }
        return;
    }
    // `--literal-links` (implied by `-a`): every link is recreated with its
    // *literal* target string — no classification, no DEST-relative rewrite,
    // no external-link dereference or skip (rsync `-l`). The source and the
    // destination probe record the same literal, so the planner quick-checks
    // them to a skip. A Windows target still materializes the link as a
    // `.lnk` (the object kind is the only cross-OS change; the receiver
    // converts the '/' wire target to '\').
    if ctx.literal_links {
        let kind = if ctx.target_os == TargetOs::Windows {
            LinkKind::Lnk
        } else {
            LinkKind::Symlink
        };
        entries.push(symlink_entry(rel.to_string(), target.to_string(), kind, lmeta));
        return;
    }
    match classify_link(link_path, target, &ctx.root) {
        LinkClass::Internal => {
            // A link that resolves to itself (`l → l`, or `sub/l → ../sub/l`)
            // is a cycle: skip it rather than mirroring a useless
            // self-referential link (spec §3.2 "loop detected"). The
            // comparison normalizes `..`/`.` so semantic self-references are
            // caught, not just literal ones. Only the source decides; a
            // destination probe still records what is there.
            if ctx.is_source {
                let resolved = crate::sync::linkpolicy::resolve_target(link_path, target);
                let self_link = crate::sync::linkpolicy::lexical_normalize(link_path);
                if crate::sync::linkpolicy::lexical_normalize(&resolved) == self_link {
                    ctx.skip(rel, "loop detected", skipped);
                    return;
                }
            }
            let kind = if ctx.target_os == TargetOs::Windows {
                LinkKind::Lnk
            } else {
                LinkKind::Symlink
            };
            // `--literal-internal-links`: keep the literal target — the
            // destination mirror is not self-contained, but the string is
            // byte-identical. Source and probe record the same literal, so
            // the quick check converges.
            if ctx.literal_internal_links {
                entries.push(symlink_entry(rel.to_string(), target.to_string(), kind, lmeta));
                return;
            }
            // Rewrite to a DEST-relative target so the destination is
            // self-contained (spec §3.2). The wire target is always
            // '/'-separated; the receiver converts to '\' when it builds a
            // `.lnk`. A Windows target cannot represent a POSIX symlink, so
            // internal links become `.lnk` shortcuts there (0 bytes).
            let target_rel = rel_of_internal_target(link_path, target, &ctx.root);
            let rewritten = rewrite_internal_target(rel, &target_rel);
            entries.push(symlink_entry(rel.to_string(), rewritten, kind, lmeta));
        }
        LinkClass::ExternalFile => {
            if !ctx.is_source {
                // Destination probe: record the link as it is. The planner
                // sees a representation mismatch (source dereferences) and
                // replaces it with a real file.
                entries.push(symlink_entry(
                    rel.to_string(),
                    target.to_string(),
                    LinkKind::Symlink,
                    lmeta,
                ));
            } else if ctx.literal_external_file_links && ctx.target_os == TargetOs::Unix {
                // `--literal-external-file-links`: keep the literal absolute
                // target (high risk — the destination must have the same
                // path).
                entries.push(symlink_entry(
                    rel.to_string(),
                    target.to_string(),
                    LinkKind::Symlink,
                    lmeta,
                ));
            } else {
                // Default: dereference — the target's content is copied as a
                // regular file. The sender opens the link path, which follows
                // to the external target, so no source-path override is needed.
                if let Ok(meta) = std::fs::metadata(link_path)
                    && meta.is_file()
                {
                    entries.push(file_entry(rel.to_string(), &meta, None));
                } else {
                    ctx.skip(rel, "unreadable external file link", skipped);
                }
            }
        }
        LinkClass::ExternalDir => {
            // `--literal-external-dir-links`: keep the link instead of
            // skipping it. The source and the probe record the same literal,
            // so the quick check converges.
            if ctx.literal_external_dir_links {
                let kind = if ctx.target_os == TargetOs::Windows {
                    LinkKind::Lnk
                } else {
                    LinkKind::Symlink
                };
                entries.push(symlink_entry(rel.to_string(), target.to_string(), kind, lmeta));
            } else if ctx.is_source {
                ctx.skip(
                    rel,
                    "external directory link (--literal-external-dir-links to keep)",
                    skipped,
                );
            }
            // Destination probe (default): record nothing — the link's
            // children are probed by their own source-named paths, and the
            // link itself is replaced by a real directory when the source
            // dereferences it.
        }
        LinkClass::DanglingExternal => {
            if ctx.is_source {
                ctx.skip(rel, "dangling external link", skipped);
            }
        }
    }
}

/// Recurse into a directory link's referent (`--follow-links`, rsync `-L`):
/// the directory becomes a real directory at the link's relative path, and
/// its entries are synced under that prefix with explicit source paths
/// outside the scan root (and marked `dereferenced`, so
/// `--remove-source-files` never touches them). Loop detection: every entered
/// directory is canonicalized and remembered; a revisit is a cycle and is
/// skipped.
fn walk_external(
    ctx: &ScanContext,
    link_path: &Path,
    rel: &str,
    entries: &mut Vec<FileEntry>,
    skipped: &mut Vec<String>,
    visited: &mut HashSet<PathBuf>,
) {
    let Ok(canonical_dir) = std::fs::canonicalize(link_path) else {
        ctx.skip(rel, "external directory link unreadable", skipped);
        return;
    };
    if !visited.insert(canonical_dir.clone()) {
        ctx.skip(rel, "loop detected", skipped);
        return;
    }
    if let Ok(meta) = std::fs::metadata(&canonical_dir) {
        entries.push(dir_entry(rel.to_string(), &meta));
    }
    walk_dir_contents(ctx, &canonical_dir, rel, entries, skipped, visited);
}

/// Walk the contents of a directory (possibly external to the scan root),
/// appending entries under `rel_prefix` with explicit source paths. The link
/// policy applies to every nested entry — internal links are still
/// re-mapped, external files dereferenced or kept, external directories
/// recursed or skipped (spec §3.2: "对内部每个条目重新应用本表").
fn walk_dir_contents(
    ctx: &ScanContext,
    dir: &Path,
    rel_prefix: &str,
    entries: &mut Vec<FileEntry>,
    skipped: &mut Vec<String>,
    visited: &mut HashSet<PathBuf>,
) {
    let mut children: Vec<_> = match std::fs::read_dir(dir) {
        Ok(it) => it.filter_map(std::result::Result::ok).collect(),
        Err(e) => {
            if ctx.warn {
                tracing::warn!("cannot read {}: {e}", dir.display());
            }
            return;
        }
    };
    children.sort_by_key(std::fs::DirEntry::file_name);
    for child in children {
        let name = child.file_name().to_string_lossy().into_owned();
        let child_path = child.path();
        let rel = if rel_prefix.is_empty() {
            name
        } else {
            format!("{rel_prefix}/{name}")
        };
        if ctx.filter.as_ref().is_some_and(|f| !f.passes(&rel)) {
            continue;
        }
        let Ok(lmeta) = std::fs::symlink_metadata(&child_path) else {
            continue;
        };
        let file_type = lmeta.file_type();
        if file_type.is_dir() {
            if !ctx.recursive {
                continue;
            }
            entries.push(dir_entry(rel.clone(), &lmeta));
            walk_dir_contents(ctx, &child_path, &rel, entries, skipped, visited);
        } else if file_type.is_file() {
            if ctx.source_is_windows && !ctx.literal_links && is_lnk_file(&child_path) {
                scan_lnk(ctx, &child_path, &rel, &lmeta, entries, skipped, visited);
            } else {
                let mut entry = file_entry(rel.clone(), &lmeta, None);
                entry.source_path = Some(child_path.clone());
                entry.dereferenced = true;
                entries.push(entry);
            }
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&child_path)
                .map(|t| wire_str(&t.to_string_lossy()))
                .unwrap_or_default();
            scan_link(ctx, &child_path, &rel, &target, Some(&lmeta), entries, skipped, visited);
        } else if let Some(kind) = special_kind(&lmeta) {
            let mut entry = special_entry(rel.clone(), &lmeta, kind);
            entry.dereferenced = true;
            entries.push(entry);
        }
    }
}

/// The target's path relative to the scan root, used to rewrite an internal
/// link's target to a DEST-relative path. Prefers the *lexical* resolution
/// (DEST mirrors the source's lexical paths); a target reached through a
/// symlinked parent chain inside the root falls back to the canonical
/// resolution; a dangling target falls back to its normalized lexical path.
fn rel_of_internal_target(link_path: &Path, target: &str, root: &Path) -> String {
    let resolved = crate::sync::linkpolicy::resolve_target(link_path, target);
    if let Ok(rel) = resolved.strip_prefix(root) {
        return wire_str(&rel.to_string_lossy());
    }
    let Ok(canon) = std::fs::canonicalize(&resolved) else {
        // Dangling: resolve the existing prefix (the base may itself be
        // reached through a symlinked path, e.g. macOS `/var`) and keep the
        // non-existing tail.
        let normalized = soft_canonicalize(&resolved)
            .unwrap_or_else(|_| crate::sync::linkpolicy::lexical_normalize(&resolved));
        return wire_str(&normalized.strip_prefix(root).unwrap_or(&resolved).to_string_lossy());
    };
    wire_str(&canon.strip_prefix(root).unwrap_or(&canon).to_string_lossy())
}

/// The on-disk source of an entry: entries pulled in through `--follow-links`
/// recursion carry an explicit path outside the scan root; everything else
/// resolves as `root.join(relative_path)`.
fn entry_source(root: &Path, entry: &FileEntry) -> PathBuf {
    file_source(root, entry.source_path.as_deref(), Path::new(&entry.relative_path))
}

/// Whether `path` (relative name `rel`) is a Shell Link (.lnk) on this scan.
///
/// A *source* scan requires the `.lnk` extension plus the MS-SHLLINK magic
/// (`HeaderSize == 0x0000004C`): a data file whose body merely starts with the
/// magic is not a shortcut — Windows itself keys on the extension. A
/// *destination* scan keys on the peer's manifest instead: only paths the
/// source classifies as links are sniffed, by content alone, so an
/// extensionless `.lnk` (a Unix-source symlink materialized on a Windows
/// target) is recognized without misclassifying arbitrary data.
fn is_lnk_entry(path: &Path, rel: &str, ctx: &ScanContext) -> bool {
    if ctx.is_source {
        // Literal preservation (`--literal-links`/`-a`): a `.lnk` is opaque
        // data — its bytes are copied as-is, never interpreted as a shortcut.
        return !ctx.literal_links && is_lnk_file(path);
    }
    ctx.source_link_paths
        .as_ref()
        .is_some_and(|links| links.contains(rel))
        && lnk_magic_of(path)
}

/// Whether `path` is a Shell Link (.lnk): the `.lnk` extension plus the
/// MS-SHLLINK header magic (`HeaderSize == 0x0000004C`). Only consulted on a
/// Windows source, where `.lnk` files are shortcuts rather than data.
fn is_lnk_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
        && lnk_magic_of(path)
}

/// Whether `path`'s first four bytes are the Shell Link header magic
/// (`HeaderSize == 0x0000004C`, MS-SHLLINK) — a content-only check, used by
/// the destination scan's source-keyed recognition.
fn lnk_magic_of(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 4];
    let n = file.read(&mut head).unwrap_or(0);
    crate::sync::linkpolicy::is_lnk_magic(&head[..n])
}

/// Extract the target path from a Shell Link (.lnk) file: the absolute
/// target from its `LinkInfo`, or the relative path from its `StringData`
/// when no absolute path is stored. `None` when the file cannot be parsed
/// (the caller then copies it as an opaque binary).
fn extract_lnk_target(path: &Path) -> Option<String> {
    let link = lnk::ShellLink::open(path, lnk::encoding::UTF_16LE)
        .or_else(|_| lnk::ShellLink::open(path, lnk::encoding::WINDOWS_1252))
        .ok()?;
    if let Some(target) = link.link_target() {
        return Some(target);
    }
    link.string_data().relative_path().clone()
}

/// Classify a Windows-source `.lnk` shortcut (spec §3.2): an internal target
/// becomes a link entry whose rewritten, DEST-relative target is materialized
/// as a `.lnk` (Windows target) or a Unix symlink; an external target (e.g.
/// `C:\Windows`) is copied as an opaque binary — its body travels as ordinary
/// file content and is never parsed. `--skip-links`/`--follow-links`/
/// `--literal-links` override the policy as for symlinks.
fn scan_lnk(
    ctx: &ScanContext,
    link_path: &Path,
    rel: &str,
    lmeta: &std::fs::Metadata,
    entries: &mut Vec<FileEntry>,
    skipped: &mut Vec<String>,
    visited: &mut HashSet<PathBuf>,
) {
    // `--skip-links` wins over everything (spec §1), Windows shortcuts
    // included: a shortcut is a link — not synced, not followed. A
    // destination probe never skips: it records the `.lnk` faithfully, so a
    // representation change in the source policy converges on the next run.
    if !ctx.preserve_links {
        if ctx.is_source {
            ctx.skip(rel, "shortcut skipped (--skip-links)", skipped);
        } else {
            // Probe: record the shortcut as a link when it parses, as data
            // otherwise.
            match extract_lnk_target(link_path) {
                Some(t) => entries.push(symlink_entry(
                    rel.to_string(),
                    wire_str(&t),
                    LinkKind::Lnk,
                    Some(lmeta),
                )),
                None => entries.push(file_entry(rel.to_string(), lmeta, None)),
            }
        }
        return;
    }
    let Some(target) = extract_lnk_target(link_path) else {
        // Unparseable: treat as a plain binary file.
        entries.push(file_entry(rel.to_string(), lmeta, None));
        return;
    };
    // `--follow-links` (rsync `-L`): dereference the shortcut — its target's
    // content is copied through the shortcut's path. The content lives at
    // the *target*, not in the `.lnk` body, so the entry carries an explicit
    // source path and is marked `dereferenced` — `--remove-source-files`
    // must never remove the target. Directory targets are recursed (with
    // loop detection). A destination probe records the `.lnk` faithfully
    // instead, so the planner replaces the shortcut on the next run.
    if ctx.follow_links {
        if ctx.is_source {
            let resolved = crate::sync::linkpolicy::resolve_target(link_path, &target);
            match std::fs::metadata(&resolved) {
                Ok(meta) if meta.is_file() => {
                    let mut entry = file_entry(rel.to_string(), &meta, None);
                    entry.source_path = Some(resolved);
                    entry.dereferenced = true;
                    entries.push(entry);
                }
                Ok(meta) if meta.is_dir() => {
                    // The resolved path is the real directory (a `.lnk` is a
                    // regular file — canonicalizing the shortcut itself would
                    // be wrong).
                    walk_external(ctx, &resolved, rel, entries, skipped, visited);
                }
                _ => {
                    ctx.skip(rel, "directory or unreadable .lnk target (--follow-links)", skipped);
                }
            }
        } else {
            entries.push(symlink_entry(
                rel.to_string(),
                wire_str(&target),
                LinkKind::Lnk,
                Some(lmeta),
            ));
        }
        return;
    }
    // `--literal-links` (implied by `-a`): a Windows-source `.lnk` is opaque
    // data (never reaches this path on the source side), while a destination
    // probe records the materialized shortcut as a link with its *literal*
    // target — internal or external alike — so the planner quick-checks it
    // against the source's literal and skips. Only the object kind (`.lnk`)
    // is fixed; the target string is untouched.
    if ctx.literal_links {
        entries.push(symlink_entry(
            rel.to_string(),
            wire_str(&target),
            LinkKind::Lnk,
            Some(lmeta),
        ));
        return;
    }
    match classify_link(link_path, &target, &ctx.root) {
        LinkClass::Internal => {
            let kind = if ctx.target_os == TargetOs::Windows {
                LinkKind::Lnk
            } else {
                LinkKind::Symlink
            };
            // `--literal-internal-links`: keep the literal target (wire
            // form) instead of the DEST-relative rewrite — the source and
            // the probe record the same, so the quick check converges.
            if ctx.literal_internal_links {
                entries.push(symlink_entry(rel.to_string(), wire_str(&target), kind, Some(lmeta)));
                return;
            }
            // Rewrite to a DEST-relative target (wire form, '/'-separated);
            // the receiver converts to '\' when materializing a `.lnk`.
            let target_rel = rel_of_internal_target(link_path, &target, &ctx.root);
            let rewritten = rewrite_internal_target(rel, &target_rel);
            entries.push(symlink_entry(rel.to_string(), rewritten, kind, Some(lmeta)));
        }
        _ => {
            // External (or dangling) target: copy the `.lnk` body as data.
            entries.push(file_entry(rel.to_string(), lmeta, None));
        }
    }
}

fn mode_from_meta(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
    }
}

/// The device number of a block/char device (Unix); `None` otherwise.
#[cfg(unix)]
#[expect(clippy::unnecessary_wraps)]
fn rdev_of(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.rdev())
}
#[cfg(not(unix))]
fn rdev_of(_meta: &std::fs::Metadata) -> Option<u64> {
    None
}

/// Classify a non-regular, non-link file as a special (fifo/socket/device)
/// from its `st_mode` type bits. `None` on platforms without such objects.
#[cfg(unix)]
fn special_kind(meta: &std::fs::Metadata) -> Option<FileKind> {
    use std::os::unix::fs::MetadataExt;
    // `mode_t` is u32 on Linux/NetBSD but u16 on Apple/FreeBSD, and the
    // `S_IF*` constants follow the platform type — compare in `mode_t`.
    let mode = libc::mode_t::try_from(meta.mode()).expect("mode fits mode_t");
    match mode & libc::S_IFMT {
        libc::S_IFIFO => Some(FileKind::Fifo),
        libc::S_IFSOCK => Some(FileKind::Socket),
        libc::S_IFBLK => Some(FileKind::BlockDevice),
        libc::S_IFCHR => Some(FileKind::CharDevice),
        _ => None,
    }
}
#[cfg(not(unix))]
fn special_kind(_meta: &std::fs::Metadata) -> Option<FileKind> {
    None
}

/// The file's inode (Unix), used to detect hard-link groups. `None` where
/// inodes do not exist (Windows) — hard links are then not preserved.
//
// `Option` is the portable contract: on any single platform this is always
// `Some` (unix) or always `None` (elsewhere), so the wrap is cfg-dependent.
#[allow(clippy::unnecessary_wraps)]
fn inode_of(meta: &std::fs::Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    // The manifest keeps i64 for the wire path; the canonical u64 conversion
    // clamps pre-epoch times to 0 and can only exceed i64::MAX past ~292
    // billion years, so the clamp below is unreachable in practice.
    i64::try_from(crate::platform::fs::mtime_secs(meta)).unwrap_or(i64::MAX)
}

/// The file's mtime nanosecond remainder — always recorded so `-a` (archive)
/// can restore sub-second fidelity; the default applies whole seconds only.
fn mtime_nsecs(meta: &std::fs::Metadata) -> u32 {
    crate::platform::fs::mtime_nsecs(meta)
}

/// The file's last-access time in seconds since the Unix epoch — always
/// recorded, restored on the receiver only under `--atimes`.
fn atime_secs(meta: &std::fs::Metadata) -> i64 {
    i64::try_from(crate::platform::fs::atime_secs(meta)).unwrap_or(i64::MAX)
}

/// The file's atime nanosecond remainder. See [`atime_secs`].
fn atime_nsecs(meta: &std::fs::Metadata) -> u32 {
    crate::platform::fs::atime_nsecs(meta)
}

/// The file's owner uid (Unix); `None` where ownership does not exist
/// (Windows) — `-a` restores it with a best-effort `chown`.
#[allow(clippy::unnecessary_wraps)]
fn uid_of(meta: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.uid())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// The file's owner gid (Unix); `None` on Windows. See [`uid_of`].
#[allow(clippy::unnecessary_wraps)]
fn gid_of(meta: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.gid())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Whether an lstat failure means the path cannot exist because a parent
/// symlink loops (ELOOP). The full walk never follows links, so a probed
/// path under a looping parent is absent rather than an error.
#[cfg(unix)]
fn is_symlink_loop(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::ELOOP)
}
#[cfg(not(unix))]
fn is_symlink_loop(_e: &std::io::Error) -> bool {
    false
}

/// A regular-file entry (also used for dereferenced symlinks: `--follow-links`
/// file targets and default external file links, whose content is copied).
fn file_entry(rel: String, meta: &std::fs::Metadata, hash: Option<[u8; 32]>) -> FileEntry {
    FileEntry {
        relative_path: rel,
        size: meta.len(),
        mode: mode_from_meta(meta),
        mtime_sec: mtime_secs(meta),
        mtime_nsec: mtime_nsecs(meta),
        file_hash: hash,
        is_dir: false,
        link_target: None,
        link_kind: LinkKind::Symlink,
        inode: inode_of(meta),
        kind: FileKind::File,
        rdev: None,
        uid: uid_of(meta),
        gid: gid_of(meta),
        atime_sec: atime_secs(meta),
        atime_nsec: atime_nsecs(meta),
        xattrs: None,
        source_path: None,
        dereferenced: false,
    }
}

/// An empty-directory entry.
fn dir_entry(rel: String, meta: &std::fs::Metadata) -> FileEntry {
    FileEntry {
        relative_path: rel,
        size: 0,
        mode: mode_from_meta(meta),
        mtime_sec: mtime_secs(meta),
        mtime_nsec: mtime_nsecs(meta),
        file_hash: None,
        is_dir: true,
        link_target: None,
        link_kind: LinkKind::Symlink,
        inode: inode_of(meta),
        kind: FileKind::Dir,
        rdev: None,
        uid: uid_of(meta),
        gid: gid_of(meta),
        atime_sec: atime_secs(meta),
        atime_nsec: atime_nsecs(meta),
        xattrs: None,
        source_path: None,
        dereferenced: false,
    }
}

/// A preserved link entry (its own lstat metadata, no content). The target is
/// the rewritten, DEST-relative string decided by the link policy; `kind`
/// says whether the receiver materializes a symlink or a `.lnk` shortcut.
fn symlink_entry(
    rel: String,
    target: String,
    kind: LinkKind,
    lmeta: Option<&std::fs::Metadata>,
) -> FileEntry {
    FileEntry {
        relative_path: rel,
        size: 0,
        mode: 0,
        mtime_sec: lmeta.map_or(0, mtime_secs),
        mtime_nsec: lmeta.map_or(0, mtime_nsecs),
        file_hash: None,
        is_dir: false,
        link_target: Some(target),
        link_kind: kind,
        inode: None,
        kind: FileKind::Symlink,
        rdev: None,
        uid: lmeta.and_then(uid_of),
        gid: lmeta.and_then(gid_of),
        atime_sec: lmeta.map_or(0, atime_secs),
        atime_nsec: lmeta.map_or(0, atime_nsecs),
        xattrs: None,
        source_path: None,
        dereferenced: false,
    }
}

/// A special file (fifo/socket/device) entry.
fn special_entry(rel: String, meta: &std::fs::Metadata, kind: FileKind) -> FileEntry {
    FileEntry {
        relative_path: rel,
        size: 0,
        mode: mode_from_meta(meta),
        mtime_sec: mtime_secs(meta),
        mtime_nsec: mtime_nsecs(meta),
        file_hash: None,
        is_dir: false,
        link_target: None,
        link_kind: LinkKind::Symlink,
        inode: inode_of(meta),
        kind,
        rdev: rdev_of(meta),
        uid: uid_of(meta),
        gid: gid_of(meta),
        atime_sec: atime_secs(meta),
        atime_nsec: atime_nsecs(meta),
        xattrs: None,
        source_path: None,
        dereferenced: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scan_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = Scanner::new(ScanOptions::default())
            .scan(dir.path())
            .await
            .unwrap();
        assert!(manifest.is_empty());
        assert_eq!(manifest.len(), 0);
    }

    #[tokio::test]
    async fn scan_files_and_nested() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), b"aaa")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();
        tokio::fs::write(dir.path().join("sub/b.txt"), b"bbbb")
            .await
            .unwrap();

        let manifest = Scanner::new(ScanOptions::default())
            .scan(dir.path())
            .await
            .unwrap();
        // Files plus the empty-directory marker for `sub`.
        assert_eq!(manifest.len(), 3);
        assert_eq!(manifest.total_bytes, 7);
        assert_eq!(manifest.files[0].relative_path, "a.txt");
        assert_eq!(manifest.files[0].size, 3);
        assert!(!manifest.files[0].is_dir);
        assert_eq!(manifest.files[1].relative_path, "sub");
        assert!(manifest.files[1].is_dir);
        assert_eq!(manifest.files[2].relative_path, "sub/b.txt");
        assert_eq!(manifest.files[2].size, 4);
    }

    #[test]
    fn include_root_component_helper() {
        // No trailing slash, a real last component → include the directory.
        assert!(include_root_component("dir"));
        assert!(include_root_component("./dir"));
        assert!(include_root_component("/abs/dir"));
        assert!(include_root_component("C:\\path\\dir"));
        assert!(include_root_component("user@host:backup"));
        // Trailing separator → contents mode.
        assert!(!include_root_component("dir/"));
        assert!(!include_root_component("dir\\"));
        // Empty / `.` / `..` last component → contents mode.
        assert!(!include_root_component(""));
        assert!(!include_root_component("."));
        assert!(!include_root_component("dir/."));
        assert!(!include_root_component("dir/.."));
        assert!(!include_root_component("/"));
    }

    #[tokio::test]
    async fn scan_include_root_component_prefixes_entries() {
        let dir = tempfile::tempdir().unwrap();
        let name = dir.path().file_name().unwrap().to_string_lossy().into_owned();
        tokio::fs::write(dir.path().join("a.txt"), b"aaa")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();
        tokio::fs::write(dir.path().join("sub/b.txt"), b"bbbb")
            .await
            .unwrap();

        let manifest = Scanner::new(ScanOptions {
            include_root_component: true,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        // Entries are prefixed with the root's name and rooted at the parent,
        // so the sender reads `parent/name/...` (rsync `dir DST` → `DST/dir`).
        let canon_dir = dir.path().canonicalize().unwrap();
        assert_eq!(manifest.root, canon_dir.parent().unwrap().to_path_buf());
        assert!(manifest
            .files
            .iter()
            .any(|f| f.relative_path == format!("{name}/a.txt")));
        assert!(manifest
            .files
            .iter()
            .any(|f| f.relative_path == format!("{name}/sub/b.txt")));
        // Every entry carries the prefix.
        assert!(manifest
            .files
            .iter()
            .all(|f| f.relative_path.starts_with(&format!("{name}/"))));
    }

    #[tokio::test]
    async fn scan_with_hash() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), b"hello")
            .await
            .unwrap();

        let manifest = Scanner::new(ScanOptions {
            hash: true,
            hash_workers: 2,
            filter: None,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        assert_eq!(manifest.len(), 1);
        let hash = manifest.files[0].file_hash.unwrap();
        assert_eq!(hash, *blake3::hash(b"hello").as_bytes());
    }

    #[tokio::test]
    async fn scan_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("single.txt");
        tokio::fs::write(&file, b"xyz").await.unwrap();

        let manifest = Scanner::new(ScanOptions::default())
            .scan(&file)
            .await
            .unwrap();
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest.files[0].relative_path, "single.txt");
    }

    #[tokio::test]
    async fn scan_single_file_with_hash() {
        // Regression: hashing a single-file scan used to join against the
        // file path itself, producing a nonexistent path and failing.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("single.bin");
        tokio::fs::write(&file, b"hello").await.unwrap();

        let manifest = Scanner::new(ScanOptions {
            hash: true,
            hash_workers: 2,
            filter: None,
            ..ScanOptions::default()
        })
        .scan(&file)
        .await
        .unwrap();
        assert_eq!(manifest.len(), 1);
        assert_eq!(
            manifest.files[0].file_hash,
            Some(*blake3::hash(b"hello").as_bytes())
        );
    }

    #[tokio::test]
    async fn scan_applies_filter() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.tmp"), b"x")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.txt"), b"y")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("node_modules"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("node_modules/pkg.js"), b"z")
            .await
            .unwrap();

        let filter = Some(crate::sync::FilterSet {
            excludes: vec!["*.tmp".to_string(), "node_modules".to_string()],
            includes: vec![],
        });
        let manifest = Scanner::new(ScanOptions {
            hash: false,
            hash_workers: 1,
            filter,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest.files[0].relative_path, "b.txt");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_symlink_records_target() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("target.txt"), b"data")
            .await
            .unwrap();
        symlink("target.txt", dir.path().join("link.txt")).unwrap();

        let manifest = Scanner::new(ScanOptions::default())
            .scan(dir.path())
            .await
            .unwrap();
        let link = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "link.txt")
            .unwrap();
        assert_eq!(link.link_target.as_deref(), Some("target.txt"));
        assert!(!link.is_dir);
        assert_eq!(link.size, 0);
        // The real file is a regular entry without a link target.
        let target = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "target.txt")
            .unwrap();
        assert!(target.link_target.is_none());
        assert_eq!(target.size, 4);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_hardlinks_share_inode() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("orig.txt"), b"same content")
            .await
            .unwrap();
        std::fs::hard_link(dir.path().join("orig.txt"), dir.path().join("dup.txt")).unwrap();

        let manifest = Scanner::new(ScanOptions::default())
            .scan(dir.path())
            .await
            .unwrap();
        let orig = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "orig.txt")
            .unwrap();
        let dup = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "dup.txt")
            .unwrap();
        assert!(orig.inode.is_some());
        assert_eq!(orig.inode, dup.inode);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_self_loop_link_skipped() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), b"x")
            .await
            .unwrap();
        // `l → l` (literal) and `sub/l → ../sub/l` (semantic self-reference)
        // are cycles: skipped, never mirrored as self-referential links.
        symlink("l", dir.path().join("l")).unwrap();
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();
        symlink("../sub/l", dir.path().join("sub/l")).unwrap();

        let manifest = Scanner::new(ScanOptions::default())
            .scan(dir.path())
            .await
            .unwrap();
        assert!(
            manifest
                .files
                .iter()
                .all(|f| f.relative_path != "l" && f.relative_path != "sub/l"),
            "self-looping links must not appear as entries"
        );
        assert!(manifest.skipped.contains(&"l".to_string()));
        assert!(manifest.skipped.contains(&"sub/l".to_string()));
        // Only the regular file and the (now effectively empty) `sub`
        // directory remain; `sub` survives because its own dir entry is
        // emitted before its skipped child.
        let rels: Vec<&str> = manifest
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert_eq!(rels, vec!["a.txt", "sub"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_captures_owner_and_nsec_mtime() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, b"x").await.unwrap();
        // Give the file a distinctive whole-second + sub-second mtime.
        let c = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
        let times = [
            libc::timespec {
                tv_sec: 1_600_000_000,
                tv_nsec: 123_456_789,
            },
            libc::timespec {
                tv_sec: 1_600_000_000,
                tv_nsec: 123_456_789,
            },
        ];
        assert_eq!(
            unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) },
            0
        );

        let manifest = Scanner::new(ScanOptions::default())
            .scan(dir.path())
            .await
            .unwrap();
        let entry = &manifest.files[0];
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(entry.uid, Some(meta.uid()), "owner uid is captured");
        assert_eq!(entry.gid, Some(meta.gid()), "owner gid is captured");
        assert_eq!(entry.mtime_sec, 1_600_000_000);
        assert_eq!(entry.mtime_nsec, 123_456_789, "nsec remainder is captured");
        assert_eq!(
            entry.atime_sec, 1_600_000_000,
            "the atime is always captured (restored only under -U)"
        );
        assert_eq!(entry.atime_nsec, 123_456_789);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_captures_xattrs_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, b"x").await.unwrap();
        crate::platform::fs::apply_xattrs(&file, &[("user.cp2_scan".to_string(), b"v".to_vec())])
            .expect("set the source xattr");

        // Off by default: no xattrs on the wire.
        let manifest = Scanner::new(ScanOptions::default())
            .scan(dir.path())
            .await
            .unwrap();
        assert_eq!(manifest.files[0].xattrs, None, "-X off carries nothing");

        // `-X` collects name/value pairs.
        let manifest = Scanner::new(ScanOptions {
            xattrs: true,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        let xattrs = manifest.files[0].xattrs.as_deref().unwrap();
        let (_, v) = xattrs
            .iter()
            .find(|(n, _)| n == "user.cp2_scan")
            .expect("the set attribute is collected");
        assert_eq!(v, b"v");
    }

    #[tokio::test]
    async fn scan_multi_merges_roots_under_base() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), b"aaa")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();
        tokio::fs::write(dir.path().join("sub/b.txt"), b"bbbb")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("empty")).await.unwrap();

        let roots = vec![
            dir.path().join("a.txt"),
            dir.path().join("sub"),
            dir.path().join("empty"),
        ];
        let manifest = Scanner::new(ScanOptions::default())
            .scan_multi(dir.path(), &roots)
            .await
            .unwrap();
        // Matched directories appear as entries (empty matches and their
        // metadata survive), files are rebased, everything is sorted.
        let rels: Vec<&str> = manifest
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert_eq!(rels, vec!["a.txt", "empty", "sub", "sub/b.txt"]);
        assert_eq!(manifest.total_bytes, 7);
        assert!(
            manifest
                .files
                .iter()
                .find(|f| f.relative_path == "empty")
                .unwrap()
                .is_dir
        );
        assert!(
            manifest
                .files
                .iter()
                .find(|f| f.relative_path == "sub")
                .unwrap()
                .is_dir
        );
    }

    #[tokio::test]
    async fn scan_multi_drops_whole_matches_the_filter_rejects() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("keep.txt"), b"k")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("skip.tmp"), b"s")
            .await
            .unwrap();

        let filter = Some(crate::sync::FilterSet {
            excludes: vec!["*.tmp".to_string()],
            includes: vec![],
        });
        let roots = vec![dir.path().join("keep.txt"), dir.path().join("skip.tmp")];
        let manifest = Scanner::new(ScanOptions {
            hash: false,
            hash_workers: 1,
            filter,
            ..ScanOptions::default()
        })
        .scan_multi(dir.path(), &roots)
        .await
        .unwrap();
        let rels: Vec<&str> = manifest
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert_eq!(rels, vec!["keep.txt"]);
    }

    #[tokio::test]
    async fn scan_multi_rebases_deep_matches() {
        // A `**` pattern matches at different depths; each match keeps its
        // own relative position under the base.
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join("a")).await.unwrap();
        tokio::fs::write(dir.path().join("a/x.rs"), b"1")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("x.rs"), b"2").await.unwrap();

        let roots = vec![dir.path().join("a/x.rs"), dir.path().join("x.rs")];
        let manifest = Scanner::new(ScanOptions::default())
            .scan_multi(dir.path(), &roots)
            .await
            .unwrap();
        let rels: Vec<&str> = manifest
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert_eq!(rels, vec!["a/x.rs", "x.rs"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_multi_keeps_symlink_match_as_link() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("real.txt"), b"data")
            .await
            .unwrap();
        symlink("real.txt", dir.path().join("link.txt")).unwrap();

        let roots = vec![dir.path().join("link.txt")];
        let manifest = Scanner::new(ScanOptions::default())
            .scan_multi(dir.path(), &roots)
            .await
            .unwrap();
        assert_eq!(manifest.len(), 1);
        let link = &manifest.files[0];
        assert_eq!(link.relative_path, "link.txt");
        assert_eq!(link.link_target.as_deref(), Some("real.txt"));
        assert!(!link.is_dir);
    }

    #[tokio::test]
    async fn scan_multi_dedupes_overlapping_roots() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), b"a")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();
        tokio::fs::write(dir.path().join("sub/b.txt"), b"b")
            .await
            .unwrap();

        // A directory, a file inside it, and the same file twice: each path
        // must appear exactly once.
        let roots = vec![
            dir.path().join("sub"),
            dir.path().join("sub/b.txt"),
            dir.path().join("a.txt"),
            dir.path().join("a.txt"),
        ];
        let manifest = Scanner::new(ScanOptions::default())
            .scan_multi(dir.path(), &roots)
            .await
            .unwrap();
        let rels: Vec<&str> = manifest
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert_eq!(rels, vec!["a.txt", "sub", "sub/b.txt"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_records_specials() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        // A fifo (mkfifo needs no privileges).
        let pipe = dir.path().join("pipe");
        let c = std::ffi::CString::new(pipe.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o640) }, 0);
        tokio::fs::write(dir.path().join("a.txt"), b"x")
            .await
            .unwrap();

        let manifest = Scanner::new(ScanOptions::default())
            .scan(dir.path())
            .await
            .unwrap();
        let pipe_entry = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "pipe")
            .unwrap();
        assert_eq!(pipe_entry.kind, crate::protocol::FileKind::Fifo);
        assert_eq!(pipe_entry.size, 0);
        let file_entry = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "a.txt")
            .unwrap();
        assert_eq!(file_entry.kind, crate::protocol::FileKind::File);
    }

    /// Build a source manifest by scanning a throwaway directory with the
    /// given layout (a helper for `scan_targeted` tests).
    async fn source_with(files: &[(&str, &[u8])], dirs: &[&str]) -> Manifest {
        let dir = tempfile::tempdir().unwrap();
        for name in dirs {
            tokio::fs::create_dir(dir.path().join(name)).await.unwrap();
        }
        for (name, content) in files {
            tokio::fs::write(dir.path().join(name), content)
                .await
                .unwrap();
        }
        Scanner::new(ScanOptions::default())
            .scan(dir.path())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn scan_targeted_probes_only_source_paths() {
        // The destination holds decoys the source never names — the manifest
        // must contain exactly the source-named paths, nothing else.
        let dest = tempfile::tempdir().unwrap();
        tokio::fs::write(dest.path().join("a.txt"), b"aaa")
            .await
            .unwrap();
        tokio::fs::write(dest.path().join("decoy.txt"), b"decoy")
            .await
            .unwrap();
        tokio::fs::create_dir(dest.path().join("sub")).await.unwrap();
        tokio::fs::write(dest.path().join("sub/b.txt"), b"bbbb")
            .await
            .unwrap();
        tokio::fs::write(dest.path().join("sub/extra.txt"), b"extra")
            .await
            .unwrap();
        tokio::fs::create_dir(dest.path().join("untouched"))
            .await
            .unwrap();
        tokio::fs::write(dest.path().join("untouched/deep.txt"), b"deep")
            .await
            .unwrap();

        let source = source_with(&[("a.txt", b"aaa"), ("sub/b.txt", b"bbbb")], &["sub"]).await;
        let manifest = Scanner::new(ScanOptions::default())
            .scan_targeted(dest.path(), &source)
            .await
            .unwrap();
        let rels: Vec<&str> = manifest
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert_eq!(rels, vec!["a.txt", "sub", "sub/b.txt"]);
        // The existing file's metadata is recorded (size + mtime), so the
        // planner's quick check can skip it.
        assert_eq!(manifest.files[0].size, 3);
        assert!(!manifest.files[0].is_dir);
        assert_eq!(manifest.files[2].size, 4);
        assert_eq!(manifest.total_bytes, 7);
    }

    #[tokio::test]
    async fn scan_targeted_prunes_absent_subtrees() {
        // The source names a whole subtree (`gone/`) that does not exist in
        // the destination; neither the dir nor its children are probed, and
        // the present source paths are still found.
        let dest = tempfile::tempdir().unwrap();
        tokio::fs::write(dest.path().join("keep.txt"), b"k")
            .await
            .unwrap();

        let source = source_with(&[("keep.txt", b"k"), ("gone/x.txt", b"x")], &["gone"]).await;
        let manifest = Scanner::new(ScanOptions::default())
            .scan_targeted(dest.path(), &source)
            .await
            .unwrap();
        let rels: Vec<&str> = manifest
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert_eq!(rels, vec!["keep.txt"]);
    }

    #[tokio::test]
    async fn scan_targeted_records_type_mismatches() {
        // A destination directory where the source has a file (and vice
        // versa) is recorded as it actually is, so the planner schedules the
        // replacement.
        let dest = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dest.path().join("x")).await.unwrap();
        tokio::fs::write(dest.path().join("y"), b"file").await.unwrap();

        // Source: `x` is a file, `y` is a directory.
        let source = source_with(&[("x", b"data")], &["y"]).await;
        let manifest = Scanner::new(ScanOptions::default())
            .scan_targeted(dest.path(), &source)
            .await
            .unwrap();
        assert_eq!(manifest.len(), 2);
        let x = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "x")
            .unwrap();
        assert!(x.is_dir, "dest has a dir where the source has a file");
        let y = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "y")
            .unwrap();
        assert!(!y.is_dir, "dest has a file where the source has a dir");
        assert_eq!(y.size, 4);
    }

    #[tokio::test]
    async fn scan_targeted_empty_source_yields_empty_manifest() {
        let dest = tempfile::tempdir().unwrap();
        tokio::fs::write(dest.path().join("only-in-dest.txt"), b"x")
            .await
            .unwrap();
        let source = source_with(&[], &[]).await;
        let manifest = Scanner::new(ScanOptions::default())
            .scan_targeted(dest.path(), &source)
            .await
            .unwrap();
        assert!(manifest.is_empty());
    }

    #[tokio::test]
    async fn scan_targeted_hashes_matched_files() {
        // `--checksum`: only source-named files are hashed (the decoy is not
        // even in the manifest), and the hash matches the file contents.
        let dest = tempfile::tempdir().unwrap();
        tokio::fs::write(dest.path().join("a.txt"), b"hello")
            .await
            .unwrap();
        tokio::fs::write(dest.path().join("decoy.txt"), b"ignored")
            .await
            .unwrap();

        let source = source_with(&[("a.txt", b"hello")], &[]).await;
        let manifest = Scanner::new(ScanOptions {
            hash: true,
            hash_workers: 2,
            filter: None,
            ..ScanOptions::default()
        })
        .scan_targeted(dest.path(), &source)
        .await
        .unwrap();
        assert_eq!(manifest.len(), 1);
        assert_eq!(
            manifest.files[0].file_hash,
            Some(*blake3::hash(b"hello").as_bytes())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_targeted_symlink_preserve_records_target() {
        use std::os::unix::fs::symlink;
        let dest = tempfile::tempdir().unwrap();
        tokio::fs::write(dest.path().join("target.txt"), b"data")
            .await
            .unwrap();
        symlink("target.txt", dest.path().join("link.txt")).unwrap();

        let source = {
            let s = tempfile::tempdir().unwrap();
            tokio::fs::write(s.path().join("target.txt"), b"data")
                .await
                .unwrap();
            symlink("target.txt", s.path().join("link.txt")).unwrap();
            Scanner::new(ScanOptions::default())
                .scan(s.path())
                .await
                .unwrap()
        };
        let manifest = Scanner::new(ScanOptions::default())
            .scan_targeted(dest.path(), &source)
            .await
            .unwrap();
        let link = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "link.txt")
            .unwrap();
        assert_eq!(link.link_target.as_deref(), Some("target.txt"));
        assert!(!link.is_dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_targeted_records_links_faithfully_under_follow() {
        use std::os::unix::fs::symlink;
        let dest = tempfile::tempdir().unwrap();
        tokio::fs::write(dest.path().join("target.txt"), b"12345")
            .await
            .unwrap();
        symlink("target.txt", dest.path().join("link.txt")).unwrap();

        // The source scan (with `--follow-links`) dereferences the
        // file-target link into a regular file entry...
        let source = {
            let s = tempfile::tempdir().unwrap();
            tokio::fs::write(s.path().join("target.txt"), b"12345")
                .await
                .unwrap();
            symlink("target.txt", s.path().join("link.txt")).unwrap();
            Scanner::new(ScanOptions {
                follow_links: true,
                ..ScanOptions::default()
            })
            .scan(s.path())
            .await
            .unwrap()
        };
        let src_link = source
            .files
            .iter()
            .find(|f| f.relative_path == "link.txt")
            .unwrap();
        assert!(
            src_link.link_target.is_none(),
            "the source scan dereferences under --follow-links"
        );

        // ...but the destination probe records the link *as it is*: the
        // planner must see the representation mismatch and replace it with a
        // real file (a stale destination link must not survive a policy
        // change).
        let manifest = Scanner::new(ScanOptions::default())
            .scan_targeted(dest.path(), &source)
            .await
            .unwrap();
        let link = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "link.txt")
            .unwrap();
        assert_eq!(link.link_target.as_deref(), Some("target.txt"));
        assert!(!link.is_dir);
        assert_eq!(link.size, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_internal_link_to_windows_target_emits_lnk_kind() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("target.txt"), b"x")
            .await
            .unwrap();
        symlink("target.txt", dir.path().join("link.txt")).unwrap();

        let manifest = Scanner::new(ScanOptions {
            target_os: TargetOs::Windows,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        let link = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "link.txt")
            .unwrap();
        // Spec §3.2: a Windows target cannot represent a POSIX symlink —
        // internal links become `.lnk` entries there.
        assert_eq!(link.link_kind, LinkKind::Lnk);
        // The wire target stays '/'-separated; the receiver converts.
        assert_eq!(link.link_target.as_deref(), Some("target.txt"));
    }

    /// Build a minimal `.lnk` at `path` carrying `target` as its relative
    /// path — exactly the shape the receiver's `create_lnk` produces
    /// (`StringData` only, UTF-16), so round-trips through `extract_lnk_target`.
    fn write_lnk_fixture(path: &Path, target: &str) {
        let mut link = lnk::ShellLink::default().with_encoding(&lnk::StringEncoding::Unicode);
        link.set_relative_path(Some(target.to_string()));
        link.save(path).expect("write .lnk fixture");
    }

    #[test]
    fn lnk_fixture_roundtrips_through_extract() {
        let dir = tempfile::tempdir().unwrap();
        let lnk_path = dir.path().join("shortcut.lnk");
        write_lnk_fixture(&lnk_path, r"..\sub\target.txt");

        let bytes = std::fs::read(&lnk_path).unwrap();
        assert!(crate::sync::linkpolicy::is_lnk_magic(&bytes));
        assert_eq!(
            extract_lnk_target(&lnk_path).as_deref(),
            Some(r"..\sub\target.txt"),
            "the relative path must survive the write→parse roundtrip"
        );
    }

    #[tokio::test]
    async fn scan_lnk_internal_target_becomes_link_entry() {
        // A Windows-source shortcut whose target is inside the scan root is
        // classified as a link (spec §3.2), not copied as a binary.
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();
        tokio::fs::write(dir.path().join("sub/target.txt"), b"x")
            .await
            .unwrap();
        write_lnk_fixture(&dir.path().join("shortcut.lnk"), r"sub\target.txt");

        let manifest = Scanner::new(ScanOptions {
            source_os: TargetOs::Windows,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        let link = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "shortcut.lnk")
            .unwrap();
        assert!(
            link.link_target.is_some(),
            "an internal .lnk must become a link entry"
        );
        assert_eq!(link.link_target.as_deref(), Some("sub/target.txt"));
        assert_eq!(link.size, 0);
    }

    #[tokio::test]
    async fn follow_links_dereferences_lnk_file_targets() {
        // `--follow-links` covers shortcuts too: a file-target .lnk is
        // dereferenced — its target's content is copied through the
        // shortcut's path, read from an explicit source path, and never
        // auto-deleted by `--remove-source-files`.
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("target.txt"), b"payload")
            .await
            .unwrap();
        write_lnk_fixture(&dir.path().join("shortcut.lnk"), r"target.txt");

        let manifest = Scanner::new(ScanOptions {
            follow_links: true,
            source_os: TargetOs::Windows,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        let entry = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "shortcut.lnk")
            .unwrap();
        assert!(
            entry.link_target.is_none(),
            "a dereferenced .lnk is a regular file"
        );
        assert_eq!(entry.size, 7);
        assert!(entry.dereferenced, "the target is not ours to remove");
        // The resolved source path is canonicalized (macOS resolves
        // `/var` → `/private/var`; Windows adds a `\\?\` prefix) — compare
        // it in the same form.
        assert_eq!(
            entry.source_path.as_ref().map(|p| p.canonicalize().unwrap()),
            Some(dir.path().join("target.txt").canonicalize().unwrap()),
            "content is read from the target, not the .lnk body"
        );
    }

    #[tokio::test]
    async fn skip_links_skips_lnk_shortcuts() {
        // `--skip-links`: a shortcut is a link — not synced, not followed.
        let dir = tempfile::tempdir().unwrap();
        write_lnk_fixture(&dir.path().join("shortcut.lnk"), r"target.txt");

        let manifest = Scanner::new(ScanOptions {
            preserve_links: false,
            source_os: TargetOs::Windows,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        assert!(
            manifest.files.iter().all(|f| f.relative_path != "shortcut.lnk"),
            "a skipped .lnk is not part of the transfer"
        );
        assert!(
            manifest.skipped.iter().any(|p| p == "shortcut.lnk"),
            "the skipped shortcut is recorded for --delete protection"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_targeted_recognizes_extensionless_lnk_when_source_says_link() {
        use std::os::unix::fs::symlink;
        // A Unix-source symlink is materialized on a Windows target as a
        // `.lnk` with the *same* (extensionless) name; the destination probe
        // must recognize it by content — keyed on the source manifest — so
        // the link is quick-check-skipped instead of re-created on every run.
        let dest = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dest.path().join("sub")).await.unwrap();
        tokio::fs::write(dest.path().join("sub/t.txt"), b"x")
            .await
            .unwrap();
        tokio::fs::create_dir(dest.path().join("links")).await.unwrap();
        write_lnk_fixture(&dest.path().join("links/l"), r"..\sub\t.txt");

        // The source: `links/l` is an internal symlink, scanned as a Unix
        // tree pushing to a Windows target (rewritten target `../sub/t.txt`).
        let src = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(src.path().join("sub")).await.unwrap();
        tokio::fs::write(src.path().join("sub/t.txt"), b"x")
            .await
            .unwrap();
        tokio::fs::create_dir(src.path().join("links")).await.unwrap();
        symlink("../sub/t.txt", src.path().join("links/l")).unwrap();
        let source = Scanner::new(ScanOptions {
            target_os: TargetOs::Windows,
            ..ScanOptions::default()
        })
        .scan(src.path())
        .await
        .unwrap();
        let src_link = source
            .files
            .iter()
            .find(|f| f.relative_path == "links/l")
            .unwrap();
        assert!(src_link.link_target.is_some());

        // The probe (a Windows destination) records the extensionless `.lnk`
        // as the same link — the planner then quick-checks it to a skip.
        let manifest = Scanner::new(ScanOptions {
            source_os: TargetOs::Windows,
            ..ScanOptions::default()
        })
        .scan_targeted(dest.path(), &source)
        .await
        .unwrap();
        let probe_link = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "links/l")
            .unwrap();
        assert!(
            probe_link.link_target.is_some(),
            "an extensionless .lnk must be recognized on the destination probe"
        );
        assert_eq!(
            probe_link.link_target, src_link.link_target,
            "the probe and the source must rewrite the target identically"
        );
        assert_eq!(probe_link.size, 0);
    }

    #[tokio::test]
    async fn full_walk_dest_scan_recognizes_extensionless_lnk_from_source_key() {
        // `--delete` uses the full destination walk; the source-keyed .lnk
        // recognition must work there too, and must leave ordinary data
        // files (whose bodies may coincidentally start with the magic)
        // untouched.
        let dest = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dest.path().join("sub")).await.unwrap();
        tokio::fs::write(dest.path().join("sub/t.txt"), b"x")
            .await
            .unwrap();
        tokio::fs::create_dir(dest.path().join("links")).await.unwrap();
        write_lnk_fixture(&dest.path().join("links/l"), r"..\sub\t.txt");

        let manifest = Scanner::new(ScanOptions {
            source_os: TargetOs::Windows,
            is_source_scan: false,
            source_link_paths: Some(["links/l".to_string()].into_iter().collect()),
            ..ScanOptions::default()
        })
        .scan(dest.path())
        .await
        .unwrap();
        let link = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "links/l")
            .unwrap();
        assert!(link.link_target.is_some());
        let file = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "sub/t.txt")
            .unwrap();
        assert!(file.link_target.is_none(), "a data file is never sniffed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn literal_scan_keeps_literal_targets_and_all_links() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("real.txt"), b"x")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("links")).await.unwrap();
        // An *absolute* internal target (default mode rewrites it to
        // DEST-relative; --literal-links keeps it literal).
        let abs_target = dir.path().join("real.txt");
        symlink(&abs_target, dir.path().join("links/l_abs")).unwrap();
        // An external directory link (default mode skips it; --literal-links
        // keeps it).
        let ext = tempfile::tempdir().unwrap();
        symlink(ext.path(), dir.path().join("links/l_ext_dir")).unwrap();
        // A self-referential link (default mode skips it as a loop;
        // --literal-links keeps it — rsync -l preserves whatever the source
        // holds).
        symlink("l_loop", dir.path().join("links/l_loop")).unwrap();

        let manifest = Scanner::new(ScanOptions {
            literal_links: true,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();

        let l_abs = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "links/l_abs")
            .unwrap();
        assert_eq!(
            l_abs.link_target.as_deref(),
            Some(abs_target.to_str().unwrap()),
            "an internal link keeps its literal absolute target under --literal-links"
        );
        assert_eq!(l_abs.link_kind, LinkKind::Symlink);
        let l_ext = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "links/l_ext_dir")
            .unwrap();
        assert!(
            l_ext.link_target.is_some(),
            "an external directory link is kept as a link under --literal-links"
        );
        let l_loop = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "links/l_loop")
            .unwrap();
        assert_eq!(
            l_loop.link_target.as_deref(),
            Some("l_loop"),
            "a self-referential link is kept as-is under --literal-links"
        );
        assert!(
            manifest.skipped.is_empty(),
            "--literal-links never skips a link"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn literal_internal_links_keeps_only_internal_target() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("real.txt"), b"x")
            .await
            .unwrap();
        tokio::fs::write(ext.path().join("e.txt"), b"y")
            .await
            .unwrap();
        let abs_target = dir.path().join("real.txt");
        symlink(&abs_target, dir.path().join("l_internal")).unwrap();
        // An external *file* link: still dereferenced under
        // --literal-internal-links (the switch covers internal links only).
        symlink(ext.path().join("e.txt"), dir.path().join("l_ext_file")).unwrap();

        let manifest = Scanner::new(ScanOptions {
            literal_internal_links: true,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        let internal = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "l_internal")
            .unwrap();
        assert_eq!(
            internal.link_target.as_deref(),
            Some(abs_target.to_str().unwrap()),
            "--literal-internal-links keeps the internal link's literal target"
        );
        let ext_file = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "l_ext_file")
            .unwrap();
        assert!(
            ext_file.link_target.is_none(),
            "the external file link is still dereferenced (default policy)"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn literal_external_file_links_keeps_external_file_target() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("real.txt"), b"x")
            .await
            .unwrap();
        tokio::fs::write(ext.path().join("e.txt"), b"y")
            .await
            .unwrap();
        // An *internal* link (relative target): still rewritten under
        // --literal-external-file-links (the switch covers external file
        // links only).
        symlink("real.txt", dir.path().join("l_internal")).unwrap();
        let abs_ext = ext.path().join("e.txt");
        symlink(&abs_ext, dir.path().join("l_ext_file")).unwrap();

        let manifest = Scanner::new(ScanOptions {
            literal_external_file_links: true,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        let ext_file = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "l_ext_file")
            .unwrap();
        assert_eq!(
            ext_file.link_target.as_deref(),
            Some(abs_ext.to_str().unwrap()),
            "--literal-external-file-links keeps the literal absolute target"
        );
        let internal = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "l_internal")
            .unwrap();
        assert_eq!(
            internal.link_target.as_deref(),
            Some("real.txt"),
            "internal links are still rewritten/kept per the default policy"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn literal_external_dir_links_keeps_external_dir_link() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        tokio::fs::write(ext.path().join("inner.txt"), b"y")
            .await
            .unwrap();
        // The link targets the external directory; under the default policy
        // it is skipped (with a warning), under --literal-external-dir-links
        // it is kept as a link with the literal target.
        let abs_ext = ext.path();
        symlink(abs_ext, dir.path().join("l_ext_dir")).unwrap();

        let manifest = Scanner::new(ScanOptions {
            literal_external_dir_links: true,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        let ext_dir = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "l_ext_dir")
            .unwrap();
        assert_eq!(
            ext_dir.link_target.as_deref(),
            Some(abs_ext.to_str().unwrap()),
            "--literal-external-dir-links keeps the external directory link"
        );
        assert!(
            manifest.skipped.is_empty(),
            "no skip is recorded for a kept external directory link"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn follow_links_dereferences_everything() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("real.txt"), b"x")
            .await
            .unwrap();
        tokio::fs::write(ext.path().join("inner.txt"), b"y")
            .await
            .unwrap();
        // An *internal* file-target link and an *external* directory link:
        // --follow-links dereferences both (files become regular entries,
        // directory referents are recursed).
        symlink("real.txt", dir.path().join("l_internal")).unwrap();
        symlink(ext.path(), dir.path().join("l_ext_dir")).unwrap();

        let manifest = Scanner::new(ScanOptions {
            follow_links: true,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        let internal = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "l_internal")
            .unwrap();
        assert!(
            internal.link_target.is_none(),
            "--follow-links dereferences the internal link into a file"
        );
        assert_eq!(internal.size, 1);
        let recursed = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "l_ext_dir/inner.txt")
            .unwrap();
        assert!(
            recursed.dereferenced,
            "the recursed entry lives outside the root and is marked"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skip_links_skips_all_links() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("real.txt"), b"x")
            .await
            .unwrap();
        symlink("real.txt", dir.path().join("l_file")).unwrap();
        symlink("real.txt", dir.path().join("l_file2")).unwrap();

        let manifest = Scanner::new(ScanOptions {
            preserve_links: false,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        assert!(
            manifest.files.iter().all(|f| f.link_target.is_none()),
            "--skip-links keeps no link in the transfer list"
        );
        assert_eq!(manifest.skipped.len(), 2, "every link is recorded as skipped");
    }

    #[tokio::test]
    async fn literal_windows_source_lnk_copied_as_opaque_data() {
        // `--literal-links`/`-a`: a Windows-source `.lnk` is opaque data — its
        // original bytes travel as a regular file, never interpreted as a
        // shortcut (which would rebuild it from a parsed target).
        let dir = tempfile::tempdir().unwrap();
        write_lnk_fixture(&dir.path().join("shortcut.lnk"), r"target.txt");

        let manifest = Scanner::new(ScanOptions {
            source_os: TargetOs::Windows,
            literal_links: true,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        let entry = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "shortcut.lnk")
            .unwrap();
        assert!(
            entry.link_target.is_none(),
            "a literal .lnk is a regular file, not a link entry"
        );
        assert!(
            entry.size > 0,
            "the shortcut body is copied verbatim"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn literal_dest_probe_records_literal_lnk_target() {
        use std::os::unix::fs::symlink;
        // Unix→Windows under `--literal-links`: the source records the
        // *literal* (non-normalized) target, the destination's materialized
        // `.lnk` is parsed back to the same literal — the planner quick-checks
        // it to a skip instead of re-creating the shortcut on every run.
        let src = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(src.path().join("sub")).await.unwrap();
        tokio::fs::write(src.path().join("sub/t.txt"), b"x")
            .await
            .unwrap();
        tokio::fs::create_dir(src.path().join("links")).await.unwrap();
        symlink("sub/../sub/t.txt", src.path().join("links/l")).unwrap();

        let source = Scanner::new(ScanOptions {
            target_os: TargetOs::Windows,
            literal_links: true,
            ..ScanOptions::default()
        })
        .scan(src.path())
        .await
        .unwrap();
        let src_link = source
            .files
            .iter()
            .find(|f| f.relative_path == "links/l")
            .unwrap();
        assert_eq!(
            src_link.link_target.as_deref(),
            Some("sub/../sub/t.txt"),
            "the source keeps the literal target — no DEST-relative rewrite"
        );

        // The Windows destination holds the materialized shortcut: the target
        // in its '\' form, exactly as `create_lnk` writes it.
        let dest = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dest.path().join("sub")).await.unwrap();
        tokio::fs::write(dest.path().join("sub/t.txt"), b"x")
            .await
            .unwrap();
        tokio::fs::create_dir(dest.path().join("links")).await.unwrap();
        write_lnk_fixture(&dest.path().join("links/l"), r"sub\..\sub\t.txt");

        let manifest = Scanner::new(ScanOptions {
            source_os: TargetOs::Windows,
            literal_links: true,
            ..ScanOptions::default()
        })
        .scan_targeted(dest.path(), &source)
        .await
        .unwrap();
        let probe_link = manifest
            .files
            .iter()
            .find(|f| f.relative_path == "links/l")
            .unwrap();
        assert_eq!(
            probe_link.link_target, src_link.link_target,
            "the probe must recover the same literal the source recorded"
        );
        assert_eq!(probe_link.size, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skip_links_overrides_literal() {
        use std::os::unix::fs::symlink;
        // `--skip-links` stays the highest priority even under
        // `--literal-links`: the link is skipped, not recreated.
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("real.txt"), b"payload")
            .await
            .unwrap();
        symlink("real.txt", dir.path().join("l")).unwrap();

        let manifest = Scanner::new(ScanOptions {
            preserve_links: false,
            literal_links: true,
            ..ScanOptions::default()
        })
        .scan(dir.path())
        .await
        .unwrap();
        assert!(
            manifest.files.iter().all(|f| f.relative_path != "l"),
            "--skip-links skips the link even under --literal-links"
        );
        assert!(
            manifest.skipped.iter().any(|p| p == "l"),
            "the skipped link is recorded for --delete protection"
        );
    }
}
