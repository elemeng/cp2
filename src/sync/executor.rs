//! Sync executor: top-level orchestration for push, pull, and serve.
//!
//! The executor owns the byte stream (the ssh stdio channel) and dispatches
//! roles; the actual work is delegated to the sender (`sync::sender`) and
//! receiver (`sync::receiver`). Decision logic stays pure (see
//! `planner`/`strategy`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::platform::storage::{StorageClass, StoragePreference};
use crate::protocol::{FileMeta, Frame, TargetOs, stream};
use crate::security::PathSanitizer;
use crate::sync::handshake;
use crate::sync::planner::{Planner, PlannerConfig};
use crate::sync::receiver::Receiver;
use crate::sync::scanner::{Manifest, ScanOptions, Scanner};
use crate::sync::sender::Sender;
use crate::sync::stats::{ItemizeAction, ItemizeEntry, SyncStats};
use crate::sync::strategy::optimal_thread_count;
use crate::sync::wire::{file_meta_from_entry, from_peer, manifest_from_file_meta};
use crate::{Error, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

/// Per-file progress reporter: `(path, done_bytes, total_bytes)`, called as
/// files are read/written and ending with `done == total` per file.
pub type ProgressFn = Arc<dyn Fn(&str, u64, u64, u64) + Send + Sync>;
// (path, done, total, files_total) — files_total is the run's transferable
// file count (the sender's plan or the receiver's manifest), so the display
// can show `[index/total]` and the remaining count.

/// IO wrapper that reports bytes moved through the progress callback
/// (`(path, done, total)`): the sender wraps its source reader (live delta
/// progress) and the receiver wraps its destination writer (apply progress).
/// One type serves both directions — the `Read`/`Write` impls are gated by
/// the inner type's capabilities.
pub(crate) struct ProgressStream<T> {
    inner: T,
    path: String,
    total: u64,
    files_total: u64,
    done: u64,
    report: ProgressFn,
}

impl<T> ProgressStream<T> {
    /// Wrap `inner`, reporting progress toward `total` for `path`; the
    /// report carries the run's transferable file count (`files_total`) so
    /// the display can show `[index/total]` and the remaining count.
    #[must_use]
    pub(crate) fn new(
        inner: T,
        path: String,
        total: u64,
        files_total: u64,
        report: ProgressFn,
    ) -> Self {
        Self {
            inner,
            path,
            total,
            files_total,
            done: 0,
            report,
        }
    }
}

impl<T: std::io::Read> std::io::Read for ProgressStream<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.done += n as u64;
            (self.report)(&self.path, self.done, self.total, self.files_total);
        }
        Ok(n)
    }
}

impl<T: std::io::Write> std::io::Write for ProgressStream<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.done += n as u64;
        (self.report)(&self.path, self.done, self.total, self.files_total);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}


/// Options controlling an executor run.
#[derive(Clone)]
// Mirrors the rsync decision flags one-to-one; grouping the booleans would
// obscure the mapping (see the CLI definition in `cli.rs`).
#[expect(clippy::struct_excessive_bools)]
pub struct ExecutorOptions {
    /// Use BLAKE3 hashes to decide changes (slower, more accurate).
    pub checksum: bool,
    /// Remove destination files not present in the source.
    pub delete: bool,
    /// Skip files where the destination is newer.
    pub update_only: bool,
    /// Skip files that already exist.
    pub ignore_existing: bool,
    /// rsync `--existing`: only update files present on the receiver, do not
    /// create new ones (directories are still created).
    pub existing: bool,
    /// rsync `--ignore-times`: transfer everything, ignoring the size+mtime
    /// quick check.
    pub ignore_times: bool,
    /// Refuse to delete more than this many files per sync (rsync
    /// `--max-delete`); `None` = unlimited.
    pub max_delete: Option<u64>,
    /// Bound `--delete` to destination paths under these wire-relative
    /// roots — the `--files-from` entries (`/data/a.txt` → `data/a.txt`):
    /// destination content outside the listed paths is left alone (rsync
    /// scopes deletes to the listed paths). Empty = the whole destination
    /// may be trimmed (`--delete` without `--files-from`, or a glob source,
    /// where rsync deletes the unmatched destination content).
    pub delete_scope: Vec<String>,
    /// Keep the replaced destination file as `<name>~` (rsync `--backup`).
    pub backup: bool,
    /// Explicit worker count for transfer + hashing (`-j`). `None` tunes the
    /// count automatically from the target storage class (see [`Self::storage`]).
    pub jobs: Option<usize>,
    /// Storage class used for automatic worker tuning. `Auto` (the default)
    /// detects the class of the filesystem being written (receiver) or hashed
    /// (source tree); an explicit `Hdd`/`Ssd` skips detection.
    pub storage: StoragePreference,
    /// Compress data frames with lz4 (`-z`).
    pub compress: bool,
    /// Bandwidth cap in bytes/second (None = unlimited).
    pub bwlimit: Option<u64>,
    /// Exclude paths matching these globs (applies to the source tree).
    pub exclude: Vec<String>,
    /// Include paths matching these globs, overriding `exclude`.
    pub include: Vec<String>,
    /// Collect per-file change entries (`--itemize-changes` / `-i`) for the
    /// run summary. Off by default so a plain run does not allocate a
    /// manifest-scale list it never prints.
    pub itemize: bool,
    /// fsync every received file before it is renamed into place (receiver side).
    pub fsync: bool,
    /// Remote target path: empty = the serve root (account home), a leading
    /// `/` = absolute, otherwise relative to the serve root. Set by the CLI
    /// from the `path` in `user@host:path`.
    pub remote_path: String,
    /// Multiple remote paths for a `--files-from` pull (each an absolute
    /// server path, mirroring the local files-from layout). Empty for a
    /// normal single-path pull.
    pub remote_paths: Vec<String>,
    /// Keep partial files at the destination when a transfer aborts
    /// (rsync `-P`); the next run delta-resumes against them.
    pub partial: bool,
    /// Use the rsync-style rollsum delta engine (fixed blocks + byte-sliding
    /// scan) instead of `FastCDC`. Both peers must agree — the flag is
    /// forwarded through the server argv.
    pub rollsum: bool,
    /// Suppress the non-error output (rsync `-q`): the per-file listing, the
    /// transfer summary, the deploy and watch lines. Errors and the skipped
    /// file report still print. Forwarded to the server so its own summary is
    /// silenced too.
    pub quiet: bool,
    /// Remove source files after they have been transferred successfully
    /// (rsync `--remove-source-files`): move-off workflows (capture
    /// instruments) where the source disk must be freed once the data is on
    /// the destination. The receiver hashes each applied file and the sender
    /// deletes a source only when the hashes match (BLAKE3). Directories and
    /// symlinks are never removed; a file whose destination apply was skipped
    /// is kept.
    pub remove_source_files: bool,
    /// Verify, after the transfer, that the destination bytes match the
    /// source (BLAKE3, computed on the fly — no re-reads). Reports any
    /// mismatch as a skipped file (exit 23) but deletes nothing. Only files
    /// this run transferred are verified; use `--checksum` to re-hash a whole
    /// tree that was already in sync. Implies per-file fsync (a verified
    /// file is also durable).
    pub verify: bool,
    /// Full rsync `-a`: additionally preserve owner, group, and special files
    /// (fifos, sockets, devices) — Unix-like systems only; silently skipped
    /// on Windows, where the Unix ownership/device model does not exist.
    /// Implies [`Self::literal_links`].
    pub archive: bool,
    /// Keep links and shortcuts as they are (rsync `-l` semantics): every
    /// symlink is recreated with its literal target string (no DEST-relative
    /// rewriting, no external-link dereference or skip) and Windows-source
    /// `.lnk` shortcuts are copied as opaque files. On a Windows target a
    /// symlink still materializes as a `.lnk`, but the target stays literal.
    /// Implied by `-a`; `--skip-links`/`--follow-links` override it.
    pub literal_links: bool,
    /// Keep *internal* links with their literal target string instead of the
    /// DEST-relative rewrite (self-contained mirrors); the external-link
    /// policy is unchanged.
    pub literal_internal_links: bool,
    /// Keep external *file-target* links as links with their literal target
    /// instead of dereferencing them; ignored when the target is Windows.
    pub literal_external_file_links: bool,
    /// Keep external *directory-target* links as links with their literal
    /// target instead of skipping them.
    pub literal_external_dir_links: bool,
    /// Recursive sync (rsync `-r`). When off, only the source root's direct
    /// files are synced; subdirectories are skipped.
    pub recursive: bool,
    /// Preserve symlinks as links (rsync `-l`). When off (`--skip-links`),
    /// every symlink and shortcut is skipped entirely — not synced, not
    /// followed.
    pub preserve_links: bool,
    /// Dereference every symlink (`--follow-links`, rsync `-L`): the target's
    /// content is copied in the link's place — file targets as regular files,
    /// directory targets recursed with loop detection.
    pub follow_links: bool,
    /// Write files sparsely (`--sparse`, rsync `-S`): runs of zeros of at
    /// least 4096 bytes become holes at the destination instead of allocated
    /// blocks (VM images, database files). Content bytes are unchanged.
    pub sparse: bool,
    /// Copy extended attributes (`--xattrs`, rsync `-X`): name/value pairs
    /// for files and directories, best-effort (symlinks are not covered).
    pub xattrs: bool,
    /// Restore the source's last-access time (`--atimes`, rsync `-U`);
    /// otherwise the receiver's atime is left alone (`UTIME_OMIT`).
    pub atimes: bool,
    /// Preserve permission bits (rsync `-p`). When off, the sender computes
    /// explicit 0644/0755 defaults instead of source-derived bits (spec
    /// §2.2) and the Windows-source `exec_hint` heuristic is disabled.
    pub preserve_perms: bool,
    /// Preserve modification times (rsync `-t`). When off, the destination
    /// gets the transfer time and the quick check falls back to size-only
    /// (rsync's behavior — size+mtime comparison would be meaningless).
    pub preserve_times: bool,
    /// The *target* side's OS, driving the permission matrix and link
    /// representation at scan time (spec §2.2 / §3.2). The source side
    /// decides; the receiver never re-derives it. Set by the CLI: the probed
    /// remote OS on push, the local OS on pull (reported to the server in the
    /// `PullRequest`), and the local OS for local copies.
    pub target_os: TargetOs,
    /// Per-file progress reporter: `(path, done_bytes, total_bytes)`. Called
    /// as files are read/written, ending with `done == total` per file.
    /// rsync's `-P`-style listing + progress comes from here (the CLI installs
    /// a renderer by default).
    pub progress: Option<ProgressFn>,
    /// rsync trailing-slash semantics for the *source*: when the source is a
    /// directory and its path had no trailing slash, the directory's own name
    /// is recreated at the destination (`cp2 dir DST` → `DST/dir/*`). Set only
    /// on the sending side; a destination scan never carries it (the receiver
    /// just applies the paths the sender names). Round-trips to the remote
    /// pull sender through the [`Frame::PullRequest`] `include_root` field.
    pub include_root_component: bool,
}

/// Build the scanner options for a tree scan from the run options; the
/// differing fields (filter, scan side, perms, link paths) are explicit.
fn scanner_options(
    options: &ExecutorOptions,
    root: &Path,
    filter: Option<crate::sync::FilterSet>,
    is_source_scan: bool,
    no_perms: bool,
    source_link_paths: Option<std::collections::HashSet<String>>,
) -> ScanOptions {
    ScanOptions {
        hash: options.checksum,
        hash_workers: resolve_hash_workers(options, root),
        filter,
        recursive: options.recursive,
        preserve_links: options.preserve_links,
        follow_links: options.follow_links,
        literal_links: options.literal_links,
        literal_internal_links: options.literal_internal_links,
        literal_external_file_links: options.literal_external_file_links,
        literal_external_dir_links: options.literal_external_dir_links,
        target_os: options.target_os,
        is_source_scan,
        no_perms,
        source_os: crate::sync::scanner::local_os(),
        source_link_paths,
        archive: options.archive,
        xattrs: options.xattrs,
        include_root_component: is_source_scan && options.include_root_component,
    }
}

/// The pull-request frame for the run options; `watch`/`watch_delay_ms` are
/// the only fields the one-shot and server-driven watch pulls differ on.
fn pull_request(options: &ExecutorOptions, watch: bool, watch_delay_ms: u32) -> Frame {
    // A `--files-from` pull carries multiple absolute paths (mirrored under
    // the destination from the filesystem root); a normal pull sends the one
    // `user@host:path` (with its trailing-slash `include_root` flag). Globs
    // in a single path are expanded by the server.
    let (paths, include_root) = if options.remote_paths.is_empty() {
        (vec![options.remote_path.clone()], crate::sync::scanner::include_root_component(&options.remote_path))
    } else {
        (options.remote_paths.clone(), false)
    };
    Frame::PullRequest {
        paths,
        excludes: options.exclude.clone(),
        includes: options.include.clone(),
        checksum: options.checksum,
        delete: options.delete,
        delete_scope: options.delete_scope.clone(),
        update_only: options.update_only,
        ignore_existing: options.ignore_existing,
        existing: options.existing,
        ignore_times: options.ignore_times,
        watch,
        watch_delay_ms,
        compress: options.compress,
        bwlimit: options.bwlimit,
        client_os: options.target_os,
        include_root,
    }
}


impl Default for ExecutorOptions {
    fn default() -> Self {
        Self {
            checksum: false,
            delete: false,
            update_only: false,
            ignore_existing: false,
            existing: false,
            ignore_times: false,
            max_delete: None,
            delete_scope: Vec::new(),
            backup: false,
            jobs: None,
            storage: StoragePreference::Auto,
            compress: false,
            bwlimit: None,
            exclude: Vec::new(),
            include: Vec::new(),
            itemize: false,
            fsync: false,
            remote_path: String::new(),
            remote_paths: Vec::new(),
            partial: true,
            rollsum: false,
            quiet: false,
            remove_source_files: false,
            verify: false,
            archive: false,
            literal_links: false,
            literal_internal_links: false,
            literal_external_file_links: false,
            literal_external_dir_links: false,
            recursive: true,
            preserve_links: true,
            follow_links: false,
            sparse: false,
            xattrs: false,
            atimes: false,
            preserve_perms: true,
            preserve_times: true,
            target_os: TargetOs::Unix,
            progress: None,
            include_root_component: false,
        }
    }
}

/// Executes sync plans over a single bidirectional byte stream.
pub struct Executor {
    send: Box<dyn AsyncWrite + Unpin + Send>,
    recv: Box<dyn AsyncRead + Unpin + Send>,
}

impl Executor {
    /// Create a new executor over the given stream halves.
    #[must_use]
    /// Create an executor over any byte stream (ssh stdio, a local server
    /// child, a russh stream).
    pub fn new(
        send: Box<dyn AsyncWrite + Unpin + Send>,
        recv: Box<dyn AsyncRead + Unpin + Send>,
    ) -> Self {
        Self { send, recv }
    }

    /// Push `local` to the peer: scan → handshake → exchange manifests →
    /// plan → apply.
    ///
    /// # Errors
    ///
    /// Returns an error on transport, I/O, or protocol failure.
    pub async fn push(&mut self, local: &Path, options: &ExecutorOptions) -> Result<SyncStats> {
        let source_manifest = scan_tree(local, options, true).await?;
        // `manifest.root` (not `local`): for a single-file source it is the
        // parent directory, so entries resolve as `parent/name`.
        self.push_scanned(&source_manifest.root, &source_manifest, options)
            .await
    }

    /// Push several source roots as the top-level entries of one sync
    /// (glob-expanded sources): every root in `roots` is scanned and merged
    /// under `base`, so all matches share a single plan, summary, and
    /// `--delete` pass. `base` is the pattern's static-prefix directory —
    /// entries are named relative to it, so `base.join(entry)` is the source
    /// file on disk.
    ///
    /// # Errors
    ///
    /// Returns an error on transport, I/O, or protocol failure.
pub async fn push_multi(
        &mut self,
        base: &Path,
        roots: &[PathBuf],
        options: &ExecutorOptions,
    ) -> Result<SyncStats> {
        let scanner = Scanner::new(scanner_options(
            options,
            base,
            Some(crate::sync::FilterSet {
                includes: options.include.clone(),
                excludes: options.exclude.clone(),
            }),
            true,
            !options.preserve_perms,
            None,
        ));
        let source_manifest = scanner.scan_multi(base, roots).await?;
        self.push_scanned(base, &source_manifest, options).await
    }

    /// Handshake and run the sender over an already-scanned manifest whose
    /// entries live under `source_root` (shared by [`Self::push`] and
    /// [`Self::push_multi`]).
    async fn push_scanned(
        &mut self,
        source_root: &Path,
        source_manifest: &Manifest,
        options: &ExecutorOptions,
    ) -> Result<SyncStats> {
        tracing::info!(
            "push scan: {} files, {} bytes",
            source_manifest.len(),
            source_manifest.total_bytes
        );

        let mut ctrl_send = self.send.as_mut();
        let mut ctrl_recv = self.recv.as_mut();
        handshake::client(&mut ctrl_send, &mut ctrl_recv).await?;

        let sender = Sender::new(
            options.bwlimit,
            options.progress.clone(),
            resolve_apply_jobs(options, source_root),
            options.remove_source_files || options.verify,
            options.rollsum,
            options.itemize,
        );
        let stats = sender
            .send(
                &mut ctrl_send,
                &mut ctrl_recv,
                source_manifest,
                source_root,
                options,
            )
            .await?;

        Ok(stats)
    }

    /// Pull a tree from the peer into `local` (rsync-style pull).
    ///
    /// Sends a [`Frame::PullRequest`], receives the peer's manifest, then
    /// applies the deltas the peer computes against our local files.
    ///
    /// # Errors
    ///
    /// Returns an error on transport, I/O, or protocol failure.
    pub async fn pull(&mut self, local: &Path, options: &ExecutorOptions) -> Result<SyncStats> {
        let start = Instant::now();

        // The pull target may not exist yet; rsync creates it.
        tokio::fs::create_dir_all(local).await.map_err(Error::Io)?;

        let mut ctrl_send = self.send.as_mut();
        let mut ctrl_recv = self.recv.as_mut();
        handshake::client(&mut ctrl_send, &mut ctrl_recv).await?;

        stream::send_frame(&mut ctrl_send, &pull_request(options, false, 0)).await?;

        // The peer plays the sender: it sends its source manifest.
        let (source_files, verify) = match stream::receive_frame(&mut ctrl_recv).await? {
            Frame::IndexRequest {
                file_list,
                verify,
                ..
            } => (file_list, verify),
            Frame::Error { message } => return Err(Error::Other(format!("Peer error: {message}"))),
            _ => return Err(Error::Other("Expected IndexRequest from peer".to_string())),
        };

        // We are the receiver: scan our destination and apply the recipes.
        let dest_manifest = scan_dest(local, options, &source_files).await?;
        // `--itemize-changes` on a pull: the sender runs on the peer, so its
        // plan is not in this process. The inputs are — the received file
        // list and our destination scan — so reproduce the same plan to
        // derive the change entries (deletes are best-effort; the sender
        // applies its own `--delete` policy filter that this copy cannot
        // fully mirror without the source's skipped-set).
        let changes = if options.itemize {
            let planner = Planner::new(PlannerConfig {
                checksum: options.checksum,
                delete: options.delete,
                delete_scope: (!options.delete_scope.is_empty())
                    .then(|| options.delete_scope.clone()),
                update_only: options.update_only,
                ignore_existing: options.ignore_existing,
                existing: options.existing,
                ignore_times: options.ignore_times,
                size_only: !options.preserve_times,
                preserve_perms: options.preserve_perms,
                preserve_times: options.preserve_times,
            });
            let source_manifest = manifest_from_file_meta(&source_files);
            planner.plan(&source_manifest, &dest_manifest).itemize()
        } else {
            Vec::new()
        };
        let receiver = Receiver::new(
            local,
            options,
            resolve_apply_jobs(options, local),
            verify,
        )?;
        let mut stats = receiver
            .receive(&mut ctrl_send, &mut ctrl_recv, source_files, &dest_manifest)
            .await?;
        stats.changes = changes;
        tracing::info!(
            "pull complete: {} files, {} bytes in {:?}",
            stats.files_received,
            stats.bytes_transferred,
            start.elapsed()
        );

        Ok(stats)
    }

    /// List a remote path without transferring (`--list-only` on a remote
    /// source): handshake, send a [`Frame::ListRequest`], and return the
    /// server's listing as change entries (a `Skip` itemize line per entry,
    /// with the file size attached, so the report path and storage work
    /// unchanged).
    ///
    /// # Errors
    ///
    /// Returns an error on transport, I/O, or protocol failure.
    pub async fn list(
        &mut self,
        remote_path: &str,
        options: &ExecutorOptions,
    ) -> Result<SyncStats> {
        let start = Instant::now();
        let mut ctrl_send = self.send.as_mut();
        let mut ctrl_recv = self.recv.as_mut();
        handshake::client(&mut ctrl_send, &mut ctrl_recv).await?;

        stream::send_frame(
            &mut ctrl_send,
            &Frame::ListRequest {
                path: remote_path.to_string(),
                excludes: options.exclude.clone(),
                includes: options.include.clone(),
            },
        )
        .await?;

        let file_list = match stream::receive_frame(&mut ctrl_recv).await? {
            Frame::ListResponse { file_list } => file_list,
            Frame::Error { message } => return Err(Error::Other(format!("Peer error: {message}"))),
            other => return Err(Error::Other(format!("Expected ListResponse, got {other:?}"))),
        };
        let changes = file_list
            .iter()
            .map(|m| {
                let kind = crate::sync::stats::kind_letter(m.kind, m.link_target.is_some());
                ItemizeEntry::new(ItemizeAction::Skip, m.path.clone(), kind).with_size(m.size)
            })
            .collect::<Vec<_>>();
        Ok(SyncStats {
            files_sent: 0,
            files_received: 0,
            bytes_transferred: 0,
            duration: start.elapsed(),
            changes,
            skipped: Vec::new(),
        })
    }

    /// Pull repeatedly over one session, driven by the server: the server
    /// watches its own source tree and starts an incremental cycle whenever
    /// it changes (see [`Self::serve`]).
    ///
    /// The first cycle runs immediately on connect; later cycles follow each
    /// server-side change burst. The session ends on Ctrl-C (the stream is
    /// dropped, so the server sees EOF) or when the server disconnects.
    ///
    /// # Errors
    ///
    /// Returns an error on transport, I/O, or protocol failure; a server
    /// disconnect surfaces as an EOF read error.
    pub async fn pull_watch(
        &mut self,
        local: &Path,
        options: &ExecutorOptions,
        delay: Duration,
    ) -> Result<SyncStats> {
        let start = Instant::now();

        // The pull target may not exist yet; rsync creates it.
        tokio::fs::create_dir_all(local).await.map_err(Error::Io)?;

        let mut ctrl_send = self.send.as_mut();
        let mut ctrl_recv = self.recv.as_mut();
        handshake::client(&mut ctrl_send, &mut ctrl_recv).await?;

        stream::send_frame(
            &mut ctrl_send,
            &pull_request(
                options,
                true,
                u32::try_from(delay.as_millis()).unwrap_or(u32::MAX),
            ),
        )
        .await?;

        let mut total = SyncStats::default();
        loop {
            tokio::select! {
                frame = stream::receive_frame(&mut ctrl_recv) => {
                    let frame = from_peer(frame?)?;
                    match frame {
                        Frame::IndexRequest {
                            file_list,
                            verify,
                            ..
                        } => {
                            // One cycle: report our destination, apply the
                            // recipes, ack — then wait for the next burst.
                            let dest_manifest = scan_dest(local, options, &file_list).await?;
                            let receiver = Receiver::new(
                                local,
                                options,
                                resolve_apply_jobs(options, local),
                                verify,
                            )?;
                            let stats = receiver
                                .receive(&mut ctrl_send, &mut ctrl_recv, file_list, &dest_manifest)
                                .await?;
                            total.files_received += stats.files_received;
                            total.bytes_transferred += stats.bytes_transferred;
                            tracing::info!(
                                "watch pull cycle: {} files, {} bytes",
                                stats.files_received,
                                stats.bytes_transferred
                            );
                        }
                        Frame::Error { message } => {
                            return Err(Error::Other(format!("Peer error: {message}")))
                        }
                        other => {
                            return Err(Error::Other(format!(
                                "Unexpected frame in pull-watch: {other:?}"
                            )))
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    // Dropping the session closes the stream and the server
                    // exits on EOF. Cancelling a mid-frame read here is fine:
                    // the stream is never reused after this point.
                    break;
                }
            }
        }
        total.duration = start.elapsed();
        Ok(total)
    }

    /// Serve a connection from the peer.
    ///
    /// Handles both directions:
    /// - a **push** arrives as `IndexRequest` (we are the receiver), or
    /// - a **pull** arrives as `PullRequest` (we are the sender).
    ///
    /// # Errors
    ///
    /// Returns an error on transport, I/O, or protocol failure.
    pub async fn serve(&mut self, root: &Path, options: &ExecutorOptions) -> Result<SyncStats> {
        let start = Instant::now();

        let mut ctrl_send = self.send.as_mut();
        let mut ctrl_recv = self.recv.as_mut();
        handshake::server(&mut ctrl_send, &mut ctrl_recv).await?;

        let stats = match stream::receive_frame(&mut ctrl_recv).await? {
            // Push: the peer is the sender; we receive files into the target
            // path (relative to our serve root), creating it if needed.
            Frame::IndexRequest {
                file_list,
                path,
                verify,
            } => {
                let target = resolve_serve_path(root, &path)?;
                tokio::fs::create_dir_all(&target)
                    .await
                    .map_err(Error::Io)?;
                let dest_manifest = scan_dest(&target, options, &file_list).await?;
                let receiver = Receiver::new(
                    &target,
                    options,
                    resolve_apply_jobs(options, &target),
                    verify,
                )?;
                receiver
                    .receive(&mut ctrl_send, &mut ctrl_recv, file_list, &dest_manifest)
                    .await?
            }
            // Pull: the peer asks for a tree; we are the sender.
            Frame::PullRequest {
                paths,
                excludes,
                includes,
                checksum,
                delete,
                delete_scope,
                update_only,
                ignore_existing,
                existing,
                ignore_times,
                watch,
                watch_delay_ms,
                compress,
                bwlimit,
                client_os,
                include_root,
            } => {
                // The client's options (filters, decision flags, compression,
                // bandwidth) apply to the tree we send. Struct-update keeps
                // the frame-owned exclude/include lists instead of cloning the
                // server's and overwriting them. `target_os` is the *client's*
                // OS — the sender needs it to build the permission/link
                // matrices for the pull direction (spec §2.2 / §3.2).
                let pull_options = ExecutorOptions {
                    checksum,
                    delete,
                    delete_scope,
                    update_only,
                    ignore_existing,
                    existing,
                    ignore_times,
                    compress,
                    bwlimit,
                    exclude: excludes,
                    include: includes,
                    target_os: client_os,
                    include_root_component: include_root,
                    ..options.clone()
                };
                // A single non-glob path keeps the classic single-root scan
                // (its trailing-slash `include_root` semantics travel in the
                // frame). Multiple paths (remote `--files-from`) and globs
                // are expanded and merged under a base, like the source side
                // of `push_multi`.
                let classic = paths.len() == 1 && !remote_glob_needs_expansion(root, &paths[0])?;
                if watch {
                    // Server-driven pull-watch: watch the source tree and run
                    // an incremental cycle per change burst until the client
                    // disconnects.
                    let delay = Duration::from_millis(u64::from(watch_delay_ms));
                    let source_root = resolve_serve_path(root, &paths[0])?;
                    return serve_watch_pull(
                        &mut ctrl_send,
                        &mut ctrl_recv,
                        &source_root,
                        &pull_options,
                        delay,
                    )
                    .await;
                }
                let (source_manifest, manifest_root) = if classic {
                    let source_root = resolve_serve_path(root, &paths[0])?;
                    tracing::info!(
                        "serve pull: {} from {}",
                        if source_root.is_dir() { "dir" } else { "path" },
                        source_root.display()
                    );
                    let manifest = scan_tree(&source_root, &pull_options, true).await?;
                    (manifest, source_root)
                } else {
                    let (base, roots) = resolve_pull_roots(root, &paths)?;
                    tracing::info!(
                        "serve pull: {} roots under {}",
                        roots.len(),
                        base.display()
                    );
                    let filter = Some(crate::sync::FilterSet {
                        includes: pull_options.include.clone(),
                        excludes: pull_options.exclude.clone(),
                    });
                    let scanner = Scanner::new(scanner_options(
                        &pull_options,
                        &base,
                        filter,
                        true,
                        !pull_options.preserve_perms,
                        None,
                    ));
                    let manifest = scanner.scan_multi(&base, &roots).await?;
                    (manifest, base)
                };
                let sender = Sender::new(
                    pull_options.bwlimit,
                    options.progress.clone(),
                    resolve_apply_jobs(&pull_options, &manifest_root),
                    pull_options.remove_source_files || pull_options.verify,
                    pull_options.rollsum,
                    // The pull sender runs on the serve side; its stats (and
                    // plan) are not the client's — the client reproduces the
                    // itemize entries from the file list it receives.
                    false,
                );
                // Read source files against *the manifest's* root, not a
                // literal path: with `include_root` (a no-slash remote path)
                // the scanner re-homes entries under the parent, and a merged
                // multi-root scan names entries relative to its base.
                let mut stats = sender
                    .send(
                        &mut ctrl_send,
                        &mut ctrl_recv,
                        &source_manifest,
                        source_manifest.root.as_path(),
                        &pull_options,
                    )
                    .await?;
                stats.duration = start.elapsed();
                stats
            }
            // Remote `--list-only`: scan the requested path (with the client's
            // filters) and reply with the listing; no transfer.
            Frame::ListRequest {
                path,
                excludes,
                includes,
            } => {
                let source_root = resolve_serve_path(root, &path)?;
                let list_options = ExecutorOptions {
                    exclude: excludes,
                    include: includes,
                    ..options.clone()
                };
                let manifest = scan_tree(&source_root, &list_options, true).await?;
                let file_list = manifest
                    .files
                    .iter()
                    .map(file_meta_from_entry)
                    .collect::<Vec<_>>();
                stream::send_frame(&mut ctrl_send, &Frame::ListResponse { file_list }).await?;
                SyncStats {
                    duration: start.elapsed(),
                    ..SyncStats::default()
                }
            }
            Frame::Error { message } => return Err(Error::Other(format!("Peer error: {message}"))),
            other => {
                return Err(Error::Other(format!(
                    "Expected IndexRequest, PullRequest, or ListRequest, got {other:?}"
                )));
            }
        };

        tracing::info!("serve complete: {:?} in {:?}", stats, start.elapsed());
        Ok(stats)
    }
}

/// Scan a directory with the given hashing policy.
///
/// `apply_filter` enables the include/exclude filter — always on the source
/// side of a transfer, never on the destination scan. It doubles as the
/// source/destination marker: the permission matrix (spec §2.2) is applied
/// only to source scans.
pub(crate) async fn scan_tree(root: &Path, options: &ExecutorOptions, apply_filter: bool) -> Result<Manifest> {
    let filter = if apply_filter {
        Some(crate::sync::FilterSet {
            includes: options.include.clone(),
            excludes: options.exclude.clone(),
        })
    } else {
        None
    };
    let scanner = Scanner::new(scanner_options(
        options,
        root,
        filter,
        apply_filter,
        !options.preserve_perms,
        None,
    ));
    scanner.scan(root).await
}

/// Scan the receiver's destination for the peer's source manifest.
///
/// Without `--delete` the destination is probed only for the paths the
/// source names ([`Scanner::scan_targeted`]), so a huge destination costs
/// O(source), not O(destination) — the planner needs a destination entry
/// only to quick-check a source path. With `--delete` the full tree is
/// required: every extra that could be removed must be named, and the
/// source's directories may cover the whole root, so the complete walk is
/// used and behaves exactly as before.
async fn scan_dest(
    root: &Path,
    options: &ExecutorOptions,
    source_files: &[FileMeta],
) -> Result<Manifest> {
    // Paths the peer classifies as links: the destination scan keys its
    // `.lnk` recognition on them. A Unix-source symlink materializes as an
    // *extensionless* `.lnk` on a Windows target (the `.lnk` extension gate
    // would miss it), and an arbitrary data file whose body merely starts
    // with the `.lnk` magic must never be misclassified — the source's own
    // classification is the only safe key.
    let source_links: HashSet<String> = source_files
        .iter()
        .filter(|m| m.link_target.is_some())
        .map(|m| m.path.clone())
        .collect();
    let scanner = Scanner::new(scanner_options(
        options,
        root,
        None,
        false,
        false,
        (!source_links.is_empty()).then_some(source_links),
    ));
    if options.delete {
        return scanner.scan(root).await;
    }
    // Empty-destination fast path: a destination root with no entries at
    // all (freshly created by us, or a new directory) has nothing to
    // answer — the quick check would find every source path absent
    // anyway. Skip the per-path probe (≈ one remote stat per source
    // path); the resulting manifest is identical.
    if let Ok(mut it) = std::fs::read_dir(root)
        && it.next().is_none()
    {
        return Ok(Manifest {
            root: root.to_path_buf(),
            files: Vec::new(),
            total_bytes: 0,
            skipped: Vec::new(),
        });
    }
    let source = manifest_from_file_meta(source_files);
    scanner.scan_targeted(root, &source).await
}

/// Receiver-side apply window (concurrent file-application tasks) when the
/// target storage is a single rotating disk: parallel writers to one head
/// thrash the disk, so drop to sequential applies.
const APPLY_JOBS_HDD: usize = 1;
/// Receiver-side apply window for SSD/NVMe (or undetectable storage); the
/// long-standing default that overlaps writes across files.
const APPLY_JOBS_SSD: usize = 16;

/// The effective storage class for `root`: an explicit `--storage` override
/// wins; `Auto` detects the filesystem's class (best-effort).
fn effective_storage(options: &ExecutorOptions, root: &Path) -> StorageClass {
    match options.storage {
        StoragePreference::Hdd => StorageClass::Hdd,
        StoragePreference::Ssd => StorageClass::Ssd,
        StoragePreference::Auto => crate::platform::storage::detect_storage(root),
    }
}

/// Number of parallel hash workers for a tree rooted at `root`: an explicit
/// `-j` wins; otherwise tuned from the storage class (hashing an HDD tree is
/// seek-bound — a single worker avoids a random-read storm).
fn resolve_hash_workers(options: &ExecutorOptions, root: &Path) -> usize {
    match options.jobs {
        Some(n) => n.max(1),
        None => match effective_storage(options, root) {
            StorageClass::Hdd => 1,
            _ => optimal_thread_count(true),
        },
    }
}

/// Receiver-side apply window for a destination rooted at `root`: an explicit
/// `-j` wins; otherwise tuned from the storage class (see [`APPLY_JOBS_HDD`]).
fn resolve_apply_jobs(options: &ExecutorOptions, root: &Path) -> usize {
    let storage = effective_storage(options, root);
    let jobs = match options.jobs {
        Some(n) => n.max(1),
        None => match storage {
            StorageClass::Hdd => APPLY_JOBS_HDD,
            _ => APPLY_JOBS_SSD,
        },
    };
    tracing::info!(
        "apply window {jobs} for {} (storage {storage:?}, explicit jobs {:?})",
        root.display(),
        options.jobs
    );
    jobs
}

/// Serve a watch-mode pull: watch `source_root` and drive incremental cycles
/// (scan → send → wait for changes) over the persistent session until the
/// client disconnects.
async fn serve_watch_pull<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    ctrl_send: &mut W,
    ctrl_recv: &mut R,
    source_root: &Path,
    options: &ExecutorOptions,
    delay: Duration,
) -> Result<SyncStats> {
    let (tx, mut changes) = tokio::sync::mpsc::unbounded_channel::<()>();
    let _watcher = crate::sync::watcher::start_watcher(source_root, tx)
        .map_err(|e| Error::Other(format!("Failed to watch source: {e}")))?;

    let start = Instant::now();
    let mut total = SyncStats::default();
    loop {
        // Scan failures (a directory being removed mid-sync, a transient I/O
        // error) are retried with backoff: no frames have been sent yet, so
        // the session stays coherent. A client disconnect during the wait
        // ends the session.
        let source_manifest = loop {
            match scan_tree(source_root, options, true).await {
                Ok(manifest) => break manifest,
                Err(e) => {
                    tracing::warn!(
                        "watch pull scan failed: {e}; retrying in {:?}",
                        crate::sync::watcher::SYNC_ERROR_BACKOFF
                    );
                    if wait_for_disconnect(ctrl_recv).await? {
                        total.duration = start.elapsed();
                        return Ok(total);
                    }
                }
            }
        };
        let sender = Sender::new(
            options.bwlimit,
            options.progress.clone(),
            resolve_apply_jobs(options, source_root),
            options.remove_source_files || options.verify,
            options.rollsum,
            // Serve-side pull sender: its itemize never reaches the client.
            false,
        );
        let stats = sender
            .send(
                ctrl_send,
                ctrl_recv,
                &source_manifest,
                source_manifest.root.as_path(),
                options,
            )
            .await?;
        total.files_sent += stats.files_sent;
        total.bytes_transferred += stats.bytes_transferred;
        tracing::info!(
            "watch pull cycle: served {} files, {} bytes",
            stats.files_sent,
            stats.bytes_transferred
        );

        match wait_for_changes(ctrl_recv, &mut changes, delay).await? {
            // `Changed`: the match is the last statement in the loop, so it
            // iterates again without an explicit continue.
            WatchWait::Changed => {}
            WatchWait::Disconnected => break,
        }
    }
    total.duration = start.elapsed();
    Ok(total)
}

/// Sleep the sync-error backoff on the server side, aborting early when the
/// client disconnects (EOF on `ctrl_recv`). Returns `true` when the session
/// should end because the client is gone.
async fn wait_for_disconnect<R: AsyncRead + Unpin>(ctrl_recv: &mut R) -> Result<bool> {
    let mut probe = [0u8; 64];
    tokio::select! {
        () = tokio::time::sleep(crate::sync::watcher::SYNC_ERROR_BACKOFF) => Ok(false),
        n = ctrl_recv.read(&mut probe) => match n {
            Ok(0) => Ok(true), // client closed the connection
            Ok(_) => Err(Error::Other(
                "Unexpected data from client during scan retry".to_string(),
            )),
            Err(e) => Err(Error::Io(e)),
        },
    }
}

/// Outcome of waiting for source changes on the server.
enum WatchWait {
    /// A change burst was observed (or the debounce window expired): run the
    /// next sync cycle.
    Changed,
    /// The client closed the connection: end the watch session.
    Disconnected,
}

/// Wait for a debounced change burst on the server side of a watch-pull
/// session, aborting early when the client disconnects (EOF on `ctrl_recv`).
///
/// Changes that already arrived during the previous cycle return immediately
/// (the tree is dirty); otherwise the wait restarts its quiet window on every
/// event, capped by the shared coalesce limit so a continuous stream still
/// syncs.
async fn wait_for_changes<R: AsyncRead + Unpin>(
    ctrl_recv: &mut R,
    changes: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
    delay: Duration,
) -> Result<WatchWait> {
    // Changes during the previous cycle: sync again right away. Drain every
    // pending signal — one cycle covers the whole burst, not one cycle per
    // queued event (the same coalescing the client-side loop applies).
    let mut dirty = false;
    while changes.try_recv().is_ok() {
        dirty = true;
    }
    if dirty {
        return Ok(WatchWait::Changed);
    }
    let outcome = crate::sync::watcher::wait_debounce(changes, delay, watch_disconnect(ctrl_recv))
        .await?;
    Ok(match outcome {
        crate::sync::watcher::BurstWait::Ready => WatchWait::Changed,
        crate::sync::watcher::BurstWait::ChannelClosed
        | crate::sync::watcher::BurstWait::Aborted => WatchWait::Disconnected,
    })
}

/// The disconnect probe raced against the server's debounce wait. Resolves
/// `Ok(())` only when the client has closed the connection; any other read
/// outcome is a protocol error.
async fn watch_disconnect<R: AsyncRead + Unpin>(ctrl_recv: &mut R) -> Result<()> {
    let mut probe = [0u8; 64];
    match ctrl_recv.read(&mut probe).await {
        Ok(0) => Ok(()), // client closed the connection
        Ok(_) => Err(Error::Other(
            "Unexpected data from client during watch wait".to_string(),
        )),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Resolve a peer-supplied target path against the serve `root`.
///
/// `""` means the root itself. A path with a leading `/` (or a Windows drive
/// prefix) is an **absolute** server path — rsync semantics: the account can
/// already reach anywhere over plain ssh, so the serve root does not contain
/// it. `..` components are still rejected. Any other path is taken relative
/// to the root (`"backup"` → `root/backup`).
fn resolve_serve_path(root: &Path, path: &str) -> Result<std::path::PathBuf> {
    if path.is_empty() {
        return Ok(root.to_path_buf());
    }
    let p = Path::new(path);
    match p.components().next() {
        Some(std::path::Component::RootDir | std::path::Component::Prefix(_)) => {
            if p.components()
                .any(|c| c == std::path::Component::ParentDir)
            {
                return Err(Error::Other(format!(
                    "Unsafe path '{path}': .. is not allowed"
                )));
            }
            Ok(p.to_path_buf())
        }
        _ => sanitize_join(root, path),
    }
}

/// Join a peer-supplied relative path under root, rejecting traversal.
fn sanitize_join(root: &Path, rel: &str) -> Result<std::path::PathBuf> {
    let sanitizer =
        PathSanitizer::new(root).map_err(|e| Error::Other(format!("Path sanitizer: {e}")))?;
    sanitizer
        .join(rel)
        .map_err(|e| Error::Other(format!("Unsafe path '{rel}': {e}")))
}

/// Whether `s` contains a glob metacharacter (`*`, `?`, or `[`).
fn has_glob_metachars(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Whether a remote pull path must be glob-expanded server-side: it contains
/// metacharacters and the literal resolved path does not exist (a literal
/// path always wins — the escape hatch for names that contain `*`/`?`/`[`,
/// mirroring the source-side `expand_source`).
fn remote_glob_needs_expansion(root: &Path, pattern: &str) -> Result<bool> {
    if !has_glob_metachars(pattern) {
        return Ok(false);
    }
    let literal = resolve_serve_path(root, pattern)?;
    Ok(!literal.exists())
}

/// Resolve a multi-path pull (`--files-from`, or a glob) into `(base, roots)`
/// for a merged `scan_multi` scan.
///
/// - A single glob expands against the serve root (or its absolute prefix),
///   exactly like the source side: the static prefix before the first
///   metacharacter is the base, matches are named relative to it.
/// - Multiple paths must be absolute server paths (mirroring the local
///   `--files-from` layout) and share one filesystem root; entries are
///   mirrored under the destination from that root.
fn resolve_pull_roots(root: &Path, paths: &[String]) -> Result<(PathBuf, Vec<PathBuf>)> {
    // An empty list (a malformed peer frame) must be an error, not an
    // index-out-of-bounds panic on `roots[0]` below.
    if paths.is_empty() {
        return Err(Error::Other("pull request has no paths".to_string()));
    }
    if paths.len() == 1 {
        let (base, matches) = expand_remote_glob(root, &paths[0])?.ok_or_else(|| {
            Error::Other(format!("no files match remote source pattern '{}'", paths[0]))
        })?;
        return Ok((base, matches));
    }
    let mut roots = Vec::with_capacity(paths.len());
    for p in paths {
        if !p.starts_with('/') {
            return Err(Error::Other(
                "remote --files-from entries must be absolute paths on the server".to_string(),
            ));
        }
        roots.push(resolve_serve_path(root, p)?);
    }
    // All entries must live under one filesystem root (the local
    // `--files-from` rule), so the merged plan has a single base.
    let base = path_root(&roots[0]);
    if roots.iter().any(|r| !r.starts_with(&base)) {
        return Err(Error::Other(
            "remote --files-from entries must be on the same filesystem root".to_string(),
        ));
    }
    Ok((base, roots))
}

/// The filesystem root of an absolute path: `/` on Unix, the drive root
/// (`C:\`) on Windows.
fn path_root(path: &Path) -> PathBuf {
    path.ancestors()
        .last()
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

/// Expand a remote glob pattern (serve-root-relative or absolute) into
/// `(base, matches)` — the server mirror of the source-side `expand_source`.
///
/// Returns `Ok(None)` when the pattern has no metacharacters or names a path
/// that literally exists (the caller then treats it as a plain single path).
fn expand_remote_glob(root: &Path, pattern: &str) -> Result<Option<(PathBuf, Vec<PathBuf>)>> {
    if !has_glob_metachars(pattern) {
        return Ok(None);
    }
    let literal = resolve_serve_path(root, pattern)?;
    if literal.exists() {
        return Ok(None);
    }
    // The base: the text before the first metachar, up to the last separator.
    let meta_at = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
    let prefix = &pattern[..meta_at];
    let base_str = match prefix.rfind(['/', '\\']) {
        Some(idx) => &prefix[..idx],
        None => "",
    };
    let base_path = if base_str.is_empty() || base_str == "." {
        root.to_path_buf()
    } else if base_str.starts_with('/') {
        PathBuf::from(base_str)
    } else {
        sanitize_join(root, base_str)?
    };
    // The glob fragment relative to the base (from the first metachar on).
    let joined = base_path.join(&pattern[meta_at..]);
    let matches = glob::glob(&joined.to_string_lossy())
        .map_err(|e| Error::Other(format!("invalid glob pattern '{pattern}': {e}")))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Other(format!("glob error for '{pattern}': {e}")))?;
    if matches.is_empty() {
        return Ok(None);
    }
    let mut matches = matches;
    matches.sort();
    Ok(Some((base_path, matches)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fuzz::FuzzRng;

    fn options(jobs: Option<usize>, storage: StoragePreference) -> ExecutorOptions {
        ExecutorOptions {
            jobs,
            storage,
            ..ExecutorOptions::default()
        }
    }

    #[test]
    fn explicit_jobs_wins_over_storage() {
        let opts = options(Some(4), StoragePreference::Hdd);
        // The user's explicit -j beats the storage-tuned default on both sides.
        assert_eq!(resolve_apply_jobs(&opts, Path::new("/tmp")), 4);
        assert_eq!(resolve_hash_workers(&opts, Path::new("/tmp")), 4);
    }

    #[test]
    fn hdd_tunes_sequential() {
        let opts = options(None, StoragePreference::Hdd);
        assert_eq!(resolve_apply_jobs(&opts, Path::new("/tmp")), 1);
        assert_eq!(resolve_hash_workers(&opts, Path::new("/tmp")), 1);
    }

    #[test]
    fn ssd_tunes_default() {
        let opts = options(None, StoragePreference::Ssd);
        assert_eq!(resolve_apply_jobs(&opts, Path::new("/tmp")), APPLY_JOBS_SSD);
        assert_eq!(
            resolve_hash_workers(&opts, Path::new("/tmp")),
            optimal_thread_count(true)
        );
    }

    #[test]
    fn auto_with_unknown_storage_falls_back_to_ssd_defaults() {
        // A nonexistent root cannot be classified → Unknown → SSD defaults
        // (the long-standing behavior), never a crash.
        let opts = options(None, StoragePreference::Auto);
        let missing = Path::new("/nonexistent/cp2-tune-test");
        assert_eq!(resolve_apply_jobs(&opts, missing), APPLY_JOBS_SSD);
        assert_eq!(
            resolve_hash_workers(&opts, missing),
            optimal_thread_count(true)
        );
    }

    #[test]
    fn zero_jobs_clamps_to_one() {
        let opts = options(Some(0), StoragePreference::Auto);
        assert_eq!(resolve_apply_jobs(&opts, Path::new("/tmp")), 1);
        assert_eq!(resolve_hash_workers(&opts, Path::new("/tmp")), 1);
    }

    #[test]
    fn resolve_serve_path_relative_and_absolute() {
        let root = tempfile::tempdir().unwrap();
        // Empty path → the serve root itself.
        assert_eq!(
            resolve_serve_path(root.path(), "").unwrap(),
            root.path().to_path_buf()
        );
        // Relative paths stay contained under the root.
        assert_eq!(
            resolve_serve_path(root.path(), "backup").unwrap(),
            root.path().join("backup")
        );
        assert_eq!(
            resolve_serve_path(root.path(), "softwares/cp2").unwrap(),
            root.path().join("softwares/cp2")
        );
        // Absolute paths (rsync semantics) pass through untouched.
        assert_eq!(
            resolve_serve_path(root.path(), "/home/user/x").unwrap(),
            Path::new("/home/user/x")
        );
        // `..` is still rejected in both forms.
        assert!(resolve_serve_path(root.path(), "..").is_err());
        assert!(resolve_serve_path(root.path(), "/home/user/../x").is_err());
    }

    #[test]
    fn fuzz_pull_roots_and_remote_glob_never_panic() {
        // Arbitrary remote source patterns (globs, traversal tokens,
        // separators, unicode, NUL) must never panic the glob expansion or
        // pull-root resolution, and every accepted plan must keep its roots
        // under the returned base.
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("d1")).unwrap();
        std::fs::write(root.path().join("d1/a.txt"), b"x").unwrap();
        std::fs::write(root.path().join("d1/a.bin"), b"x").unwrap();

        let mut rng = FuzzRng::new(0x6C08_5EC7_6C0B_5EED);
        for _ in 0..1_000 {
            let pattern = if rng.below(2) == 0 {
                rng.pathish(4)
            } else {
                rng.string(32)
            };

            if let Ok(Some((base, matches))) = expand_remote_glob(root.path(), &pattern) {
                assert!(
                    matches.iter().all(|m| m.starts_with(&base)),
                    "glob {pattern:?} escaped {base:?}: {matches:?}"
                );
            }
            let _ = remote_glob_needs_expansion(root.path(), &pattern);

            // Single-path resolution (the `--files-from`/glob branch): the
            // plain-path cases return the "no files match" error by design —
            // the classic single-path pull never reaches this function.
            if let Ok((base, roots)) =
                resolve_pull_roots(root.path(), std::slice::from_ref(&pattern))
            {
                assert!(!roots.is_empty());
                assert!(
                    roots.iter().all(|r| r.starts_with(&base)),
                    "single-path {pattern:?} escaped {base:?}"
                );
            }

            // Multi-path resolution with mixed absolute/relative/glob entries.
            let n = 1 + rng.below(3);
            let paths: Vec<String> = (0..n)
                .map(|_| {
                    if rng.below(2) == 0 {
                        format!("/{}", rng.pathish(3))
                    } else {
                        rng.pathish(3)
                    }
                })
                .collect();
            if let Ok((base, roots)) = resolve_pull_roots(root.path(), &paths) {
                assert!(!roots.is_empty());
                assert!(
                    roots.iter().all(|r| r.starts_with(&base)),
                    "multi-path {paths:?} escaped {base:?}"
                );
            }
        }
    }
}
