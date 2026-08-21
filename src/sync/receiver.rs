//! Receiver role: reply to a manifest, then apply the peer's recipes — delta
//! patches and small-file batches — into a [`StagedFile`] per file, atomically.

use std::collections::{HashMap, HashSet};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::delta::{Delta, Signature, apply_patch};
use crate::platform::staging::StagedFile;
use crate::protocol::{BatchFile, FileMeta, Frame, LinkKind, LinkSpec, SignatureEntry, SkippedFile, stream};
use crate::security::PathSanitizer;
use crate::sync::executor::{ExecutorOptions, ProgressFn};
use crate::sync::scanner::Manifest;
use crate::sync::stats::SyncStats;
use crate::sync::wire::{file_meta_from_entry, from_peer};
use crate::{Error, Result};
use tokio::io::{AsyncRead, AsyncWrite};

/// Reason recorded when a destination file changed after it was applied but
/// before the hash was reported (the source must not be removed).
const DEST_CHANGED_MSG: &str = "destination changed after apply; source not removed";

/// The receiver half of a sync: applies the peer's recipes to a local root.
/// The independent boolean flags mirror the CLI one-to-one (rsync semantics);
/// grouping them would obscure the mapping.
#[expect(clippy::struct_excessive_bools)]
pub(crate) struct Receiver {
    root: PathBuf,
    sanitizer: PathSanitizer,
    /// fsync every received file before rename (opt-in via `--fsync`).
    fsync: bool,
    /// Keep partial files at the destination when a transfer aborts
    /// (rsync `-P`); the next run delta-resumes against them.
    partial: bool,
    /// Keep the replaced destination file as `<name>~` (rsync `--backup`).
    backup: bool,
    /// Refuse to delete more than this many files per sync
    /// (rsync `--max-delete`); `None` = unlimited.
    max_delete: Option<u64>,
    /// Maximum concurrent file-application tasks (overlapped disk writes
    /// across files, bounded so memory stays bounded). Tuned from the target
    /// storage class: 1 on HDD (one head, parallel writers thrash), 16 on
    /// SSD/NVMe (see `sync::executor`).
    apply_jobs: usize,
    /// Compute and report a per-file BLAKE3 hash of everything applied (set
    /// when the sender runs `--remove-source-files`, so it can verify the
    /// destination bytes before deleting the source).
    verify: bool,
    /// rsync-style rollsum delta engine (fixed blocks) instead of `FastCDC`.
    rollsum: bool,
    /// Apply modification times (rsync `-t`; off via `--no-times`).
    /// Permission bits are always applied on Unix — the *sender* computes the
    /// final mode (spec §2.2 matrix) and this receiver just executes it
    /// (0-Root: owner/group are never touched).
    preserve_times: bool,
    /// `-a` (archive): restore owner/group (best-effort `chown` — a non-root
    /// receiver keeps the SSH user's ownership, the default 0-Root model).
    /// Nanosecond mtimes are always restored.
    archive: bool,
    /// `-S` (`--sparse`): write runs of zeros as holes (lseek) instead of
    /// allocating blocks — VM images and database files stay sparse.
    sparse: bool,
    /// `-X` (`--xattrs`): apply the source's extended attributes to applied
    /// files and directories (best-effort — failures warn and keep going).
    xattrs: bool,
    /// `-U` (`--atimes`): restore the source's last-access time; otherwise
    /// the receiver's atime is left alone (`UTIME_OMIT`).
    preserve_atimes: bool,
    /// Per-file progress reporter (see `ExecutorOptions::progress`).
    progress: Option<ProgressFn>,
}

impl Receiver {
    /// Create a receiver rooted at `root` (must exist).
    ///
    /// The root is canonicalized once: the path sanitizer resolves against it
    /// *and* `prune_empty_dirs` compares against it as its upper boundary.
    /// A raw relative root (the server's serve root arrives as `.`) would
    /// never equal the absolute pruned paths, letting the prune climb above
    /// the root and delete it.
    ///
    /// `apply_jobs` caps the number of concurrently-applied file recipes
    /// (the disk-write overlap window). `verify` enables per-file hashing.
    pub(crate) fn new(
        root: &Path,
        options: &ExecutorOptions,
        apply_jobs: usize,
        verify: bool,
    ) -> Result<Self> {
        let root = root.canonicalize().map_err(|e| {
            Error::Other(format!("failed to canonicalize sync root {}: {e}", root.display()))
        })?;
        let sanitizer =
            PathSanitizer::new(&root).map_err(|e| Error::Other(format!("Path sanitizer: {e}")))?;
        Ok(Self {
            root,
            sanitizer,
            fsync: options.fsync,
            partial: options.partial,
            backup: options.backup,
            max_delete: options.max_delete,
            apply_jobs: apply_jobs.max(1),
            verify,
            rollsum: options.rollsum,
            preserve_times: options.preserve_times,
            archive: options.archive,
            sparse: options.sparse,
            xattrs: options.xattrs,
            preserve_atimes: options.atimes,
            progress: options.progress.clone(),
        })
    }

    /// Receiver role: send our manifest, then apply the peer's recipes.
    ///
    /// # Errors
    ///
    /// Returns an error on transport, I/O, or protocol failure.
    pub(crate) async fn receive<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
        &self,
        ctrl_send: &mut W,
        ctrl_recv: &mut R,
        source_files: Vec<FileMeta>,
        dest_manifest: &Manifest,
    ) -> Result<SyncStats> {
        let start = Instant::now();
        self.send_index_response(ctrl_send, dest_manifest).await?;
        let (bytes, files, skipped) = self
            .apply_recipes(ctrl_send, ctrl_recv, source_files)
            .await?;
        // Counts cannot exceed usize on any real sync.
        #[expect(clippy::cast_possible_truncation)]
        let files_received = files as usize;
        Ok(SyncStats {
            files_sent: 0,
            files_received,
            bytes_transferred: bytes,
            duration: start.elapsed(),
            // The receiver records no change entries itself; on a pull the
            // executor derives them from the reproduced plan (`options.itemize`).
            changes: Vec::new(),
            skipped,
        })
    }

    /// Send our destination manifest to the sender.
    async fn send_index_response<W: AsyncWrite + Unpin>(
        &self,
        ctrl_send: &mut W,
        dest_manifest: &Manifest,
    ) -> Result<()> {
        let file_list: Vec<FileMeta> = dest_manifest
            .files
            .iter()
            .map(file_meta_from_entry)
            .collect();
        stream::send_frame(ctrl_send, &Frame::IndexResponse { file_list }).await?;
        Ok(())
    }

    /// Receiver role: apply recipes until the peer sends `Done`, then ack.
    ///
    /// `source_meta` carries the source file metadata (mode, mtime) that is
    /// applied to each received file after it is written. It is taken by
    /// value so each entry can be shared with the apply tasks via `Arc`
    /// instead of being cloned per file.
    async fn apply_recipes<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
        &self,
        ctrl_send: &mut W,
        ctrl_recv: &mut R,
        source_meta: Vec<FileMeta>,
    ) -> Result<(u64, u64, Vec<SkippedFile>)> {
        // The run's transferable file count (the pull side's display
        // denominator — the files among the source manifest's entries).
        let files_total = source_meta
            .iter()
            .filter(|m| m.kind == crate::protocol::FileKind::File)
            .count() as u64;
        let mut state = ApplyState::new(source_meta, self.apply_jobs);

        let outcome: Result<(u64, u64, Vec<SkippedFile>)> = async move {
            let loop_result: Result<()> = async {
                loop {
                    let frame = from_peer(stream::receive_frame(ctrl_recv).await?)?;
                    match frame {
                        Frame::DeltaRecipe {
                            file_path,
                            delta,
                            source_signature,
                            basis_path,
                            ..
                        } => {
                            self.handle_delta_recipe(
                                &mut state,
                                file_path,
                                delta,
                                source_signature,
                                basis_path,
                                files_total,
                            )
                            .await?;
                        }
                        Frame::Batch { recipes } => {
                            self.handle_batch(&mut state, recipes, files_total).await?;
                        }
                        Frame::FileStart {
                            file_id,
                            file_path,
                            size,
                        } => {
                            self.handle_file_start(&mut state, file_id, &file_path, size)?;
                        }
                        Frame::FileChunk { file_id, data, .. } => {
                            self.handle_file_chunk(&mut state, file_id, data, files_total).await?;
                        }
                        Frame::FileEnd { .. } => {
                            self.handle_file_end(&mut state).await?;
                        }
                        Frame::SignatureRequest { paths } => {
                            self.handle_signature_request(ctrl_send, paths).await?;
                        }
                        Frame::MakeDir { paths } => {
                            self.handle_make_dir(&mut state, paths).await?;
                        }
                        Frame::CreateLinks {
                            links,
                            hardlinks,
                            specials,
                        } => {
                            self.handle_create_links(&mut state, links, hardlinks, specials)
                                .await?;
                        }
                        Frame::DeleteRequest { paths } => {
                            self.handle_delete_request(&mut state, paths).await?;
                        }
                        Frame::Done { files, bytes } => {
                            if self
                                .handle_done(&mut state, ctrl_send, files, bytes)
                                .await?
                            {
                                break;
                            }
                        }
                        other => {
                            return Err(Error::Other(format!(
                                "Unexpected frame in apply loop: {other:?}"
                            )));
                        }
                    }
                }
                Ok(())
            }
            .await;

            // Error path: in-flight applies are still running (their
            // JoinHandles were dropped when the loop returned early) — await
            // them so no file is committed after the sync has errored. With
            // `--partial`, the interrupted chunked file's staged temp is
            // renamed over its destination so the next run can delta-resume.
            if loop_result.is_err()
                && self.partial
                && let Some(mut f) = state.in_flight.take()
                && let Some(staged) = f.staged
            {
                // The deferred write task owns the file; join it so the
                // partial is complete before it is truncated.
                if let Some(handle) = f.pending_write.take()
                    && let Ok(Ok((file, _hasher))) = handle.await
                {
                    f.file = Some(file);
                }
                // Truncate the partial to the bytes actually written so the
                // next run's quick check (size+mtime) detects it and
                // delta-resumes.
                if let Some(file) = &mut f.file {
                    let _ = file.set_len(f.offset);
                }
                let _ = std::fs::rename(staged.path(), &f.file_path);
            }
            if let Some(handle) = state.batch_apply.take() {
                let _ = handle.await;
            }
            for handle in state.applied_this_run.drain().map(|(_, h)| h) {
                let _ = handle.await;
            }

            // Directory metadata goes last (rsync-style): files created inside
            // a directory bump its mtime, so it can only be set once
            // everything is in.
            if !state.dir_metas.is_empty() {
                let cfg = self.apply_cfg();
                tokio::task::spawn_blocking(move || {
                    for (path, meta) in state.dir_metas {
                        let _ = apply_source_meta_sync(&path, &meta, cfg);
                    }
                })
                .await
                .map_err(|e| Error::Other(format!("Dir meta task panicked: {e}")))?;
            }

            loop_result.map(|()| (state.bytes_transferred, state.files_received, state.skipped))
        }
        .await;

        outcome
    }

    /// Spawn one file-application task for a delta recipe. Used by the
    /// `DeltaRecipe` frame and by each entry of a `Batch` frame, so the
    /// in-flight bookkeeping lives in one place.
    async fn handle_delta_recipe(
        &self,
        state: &mut ApplyState,
        file_path: String,
        delta: Delta,
        source_signature: Option<Signature>,
        basis_path: Option<String>,
        files_total: u64,
    ) -> Result<()> {
        // A cross-file delta reads its basis from another file that may have
        // been applied earlier in this run: join that apply before reading.
        if let Some(basis) = &basis_path
            && let Some(handle) = state.applied_this_run.remove(basis)
        {
            handle.await.map_err(|e| {
                Error::Other(format!("basis apply task panicked: {e}"))
            })??;
        }
        let path = self.safe_join(&file_path)?;
        let basis = basis_path
            .map(|p| self.safe_join(&p))
            .transpose()?;
        let meta = state.meta_map.get(&file_path).cloned();
        let permit = state.apply_semaphore.clone().acquire_owned().await;
        let cfg = self.apply_cfg();
        let progress = self.progress.clone();
        let display = file_path;
        let map_key = display.clone();
        let handle = tokio::spawn(async move {
            let _permit = permit;
            apply_recipe(
                path,
                delta,
                source_signature,
                basis,
                meta,
                cfg,
                display,
                progress,
                files_total,
            )
            .await
        });
        state.applied_this_run.insert(map_key, handle);
        Ok(())
    }

    /// Apply a whole `Batch` frame: the files are pre-verified per unique
    /// parent (one canonicalization walk per parent instead of per file —
    /// the dominant per-file cost on small-file trees; safe within the batch,
    /// whose apply is one task with no interleaved receiver mutations) and
    /// applied by parallel sub-tasks (one per apply-window slot), each a
    /// `spawn_blocking` running the shared per-file core serially over its
    /// chunk. One semaphore permit covers the whole batch (the batch is one
    /// window slot; the sub-tasks share it). The outcomes are folded by
    /// [`drain_applies`] in any order (the counters and the hash map are
    /// path-keyed). Batch files are ≤ 2 MiB (below the cross-file
    /// basis and no `applied_this_run` entries are needed.
    async fn handle_batch(
        &self,
        state: &mut ApplyState,
        recipes: Vec<BatchFile>,
        files_total: u64,
    ) -> Result<()> {
        // Pre-verify each unique parent directory once and join the files
        // with the parent-chain walk skipped (see `join_preverified`). A
        // recipe that fails the pre-verification (a traversal attempt from
        // a corrupt or hostile peer) skips just that file — the rest of the
        // batch still applies, matching the apply core's per-file skip
        // semantics: one bad path must not abort the whole batch (and with
        // it the sync), as the file errors inside the same frame already do.
        let mut verified_parents: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();
        let mut items = Vec::with_capacity(recipes.len());
        for recipe in recipes {
            if let Some(parent) = Path::new(&recipe.file_path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                match self.sanitizer.join_preverified(&parent.to_string_lossy()) {
                    Ok(full_parent) => {
                        if verified_parents.insert(full_parent.clone())
                            && let Err(e) = self.sanitizer.verify_parent(&full_parent)
                        {
                            state.skipped.push(SkippedFile::new(
                                recipe.file_path.clone(),
                                format!("Unsafe path '{}': {e}", parent.display()),
                            ));
                            continue;
                        }
                    }
                    Err(e) => {
                        state.skipped.push(SkippedFile::new(
                            recipe.file_path.clone(),
                            format!("Unsafe path '{}': {e}", parent.display()),
                        ));
                        continue;
                    }
                }
            }
            let path = match self.sanitizer.join_preverified(&recipe.file_path) {
                Ok(path) => path,
                Err(e) => {
                    state.skipped.push(SkippedFile::new(
                        recipe.file_path.clone(),
                        format!("Unsafe path '{}': {e}", recipe.file_path),
                    ));
                    continue;
                }
            };
            let meta = state.meta_map.get(&recipe.file_path).cloned();
            items.push((path, recipe.delta, meta, recipe.file_path));
        }
        let permit = state.apply_semaphore.clone().acquire_owned().await;
        let cfg = self.apply_cfg();
        let progress = self.progress.clone();
        // The batch's sub-tasks are capped well below the apply window:
        // tiny-file applies oversubscribe the disk/allocator beyond ~4
        // concurrent writers (measured: 16-way is ~2.4x slower than 4-way
        // on this SSD). The window's full width stays for the large-file
        // delta path, where the disk parallelism matters.
        let jobs = self.apply_jobs.clamp(1, 4);
        let chunk_size = items.len().div_ceil(jobs).max(1);
        let handle = tokio::spawn(async move {
            let _permit = permit;
            let mut outcomes = Vec::with_capacity(items.len());
            let mut tasks = Vec::new();
            while !items.is_empty() {
                let take = chunk_size.min(items.len());
                let chunk: Vec<_> = items.drain(..take).collect();
                let progress = progress.clone();
                tasks.push(tokio::task::spawn_blocking(move || {
                    chunk
                        .into_iter()
                        .map(|(path, delta, meta, display)| {
                            apply_recipe_blocking(
                                path,
                                delta,
                                None,
                                None,
                                meta,
                                cfg,
                                display,
                                progress.clone(),
                                files_total,
                            )
                        })
                        .collect::<Vec<_>>()
                }));
            }
            for task in tasks {
                outcomes.extend(
                    task.await
                        .map_err(|e| Error::Other(format!("Batch apply task panicked: {e}")))?,
                );
            }
            Ok(outcomes)
        });
        state.batch_apply = Some(handle);
        Ok(())
    }

    /// Begin a sequential chunked-file transfer (`FileStart`): stage the
    /// destination so an abort can leave a resumable partial.
    fn handle_file_start(
        &self,
        state: &mut ApplyState,
        file_id: u64,
        file_path: &str,
        size: u64,
    ) -> Result<()> {
        if state.in_flight.is_some() {
            return Err(Error::Other(
                "FileStart while another file is in flight".to_string(),
            ));
        }
        let path = self.safe_join(file_path)?;
        let display = file_path.to_string();
        let meta = state.meta_map.get(file_path).cloned();
        let sparse = self.sparse;
        let staged = match StagedFile::new(&path).and_then(|s| s.prepare(size, sparse).map(|()| s)) {
            Ok(staged) => Some(staged),
            Err(e) => {
                tracing::warn!("skipping {}: {e}", path.display());
                // Wire-relative path: the sender matches skips against its
                // transfer list for `--remove-source-files`.
                state.skipped.push(SkippedFile::new(file_path.to_string(), e.to_string()));
                None
            }
        };
        let file = staged
            .as_ref()
            .and_then(|s| s.open().ok())
            .map(|f| SparseWriter::new(f, size, sparse));
        let failed = staged.is_none();
        state.in_flight = Some(ChunkedFile {
            file_id,
            staged,
            file,
            pending_write: None,
            write_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(WRITE_QUEUE_DEPTH)),
            offset: 0,
            file_path: path,
            display,
            hasher: self.verify.then(blake3::Hasher::new),
            meta,
            size,
            failed,
        });
        Ok(())
    }

    /// Append one chunk to the in-flight file (`FileChunk`). The chunk is
    /// matched to the in-flight transfer by `file_id`. A file whose staging
    /// failed is drained without writing and skipped at `FileEnd`.
    ///
    /// The disk write is deferred to a blocking task in a bounded queue: each
    /// chunk's task chains the previous one (blocking on its join inside the
    /// task), so writes stay strictly sequential and the file position and
    /// the sparse zero-run tracking are unaffected — but the loop never joins
    /// mid-stream, so a slow disk never stalls the wire. The semaphore bounds
    /// the queue (memory); the chain head is joined at `FileEnd` and on the
    /// abort path.
    async fn handle_file_chunk(
        &self,
        state: &mut ApplyState,
        file_id: u64,
        data: Vec<u8>,
        files_total: u64,
    ) -> Result<()> {
        let Some(f) = &mut state.in_flight else {
            return Err(Error::Other("FileChunk without FileStart".to_string()));
        };
        if f.file_id != file_id {
            return Err(Error::Other("FileChunk file_id mismatch".to_string()));
        }
        if f.failed {
            return Ok(()); // drain without writing
        }
        // Bound the write queue: when it is full the loop waits for the
        // oldest write, so at most `WRITE_QUEUE_DEPTH` chunks sit in memory
        // (the wait is one write's worth per full queue — 1/32 of the disk
        // exposure of a join-per-chunk pipeline).
        let permit = f.write_slots.clone().acquire_owned().await;
        let prev = f.pending_write.take();
        let file = f.file.take();
        let hasher = f.hasher.take();
        let n = data.len() as u64;
        f.offset += n;
        if let Some(report) = &self.progress {
            report(&f.display, f.offset, f.size, files_total);
        }
        // Defer the write: this task chains the previous one (blocking on
        // its join inside the blocking pool — writes stay strictly
        // sequential), and runs while the loop reads the next frame off the
        // wire. The hasher rides the chain — updates are byte-ordered (the
        // canonical sequential BLAKE3) while the hashing overlaps the wire
        // reads. A write error propagates through the chain to the final
        // join.
        f.pending_write = Some(tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let (mut file, mut hasher) = match prev {
                Some(prev) => {
                    // Block on the previous task inside this blocking-pool
                    // thread (the runtime context is available here), so the
                    // chain stays strictly ordered.
                    let (file, hasher) = tokio::runtime::Handle::current()
                        .block_on(prev)
                        .map_err(|e| {
                            std::io::Error::other(format!("chunk write task panicked: {e}"))
                        })??;
                    (file, hasher)
                }
                None => (
                    file.ok_or_else(|| {
                        std::io::Error::other("FileChunk for unopenable staged file")
                    })?,
                    hasher,
                ),
            };
            file.write_all(&data)?;
            if let Some(h) = &mut hasher {
                h.update(&data);
            }
            Ok((file, hasher))
        }));
        Ok(())
    }

    /// Finalize the in-flight file (`FileEnd`): commit the staged temp and
    /// apply the source metadata.
    async fn handle_file_end(&self, state: &mut ApplyState) -> Result<()> {
        let Some(mut f) = state.in_flight.take() else {
            return Err(Error::Other("FileEnd without FileStart".to_string()));
        };
        if f.failed {
            return Ok(()); // already skipped at FileStart
        }
        let Some(staged) = f.staged else {
            return Ok(());
        };
        // Join the last deferred write and recover the write handle (sparse
        // filter) and the hasher, so pending zeros can be flushed and the
        // hash finalized before the rename.
        if let Some(handle) = f.pending_write.take() {
            let (file, hasher) = handle
                .await
                .map_err(|e| Error::Other(format!("Chunk write task panicked: {e}")))??;
            f.file = Some(file);
            f.hasher = hasher;
        }
        let sparse_writer = f.file;
        // Verification implies durability: the sender will delete the source
        // on our word, so the file must be on disk first.
        let cfg = self.apply_cfg();
        let path = f.file_path;
        let rel = f.display;
        let hash = f.hasher.map(|h| *h.finalize().as_bytes());
        let meta = f.meta;
        // Bytes actually written (the announced size may be stale if the
        // source shrank mid-transfer; the next run's quick check re-syncs).
        let written = f.offset;
        let parent = path
            .parent()
            .map_or_else(|| path.clone(), Path::to_path_buf);
        // The commit task also re-stats the destination: a post-apply
        // modification voids the hash we are about to report.
        let durable = tokio::task::spawn_blocking(move || -> Result<bool> {
            // Flush pending zeros and truncate to the announced size before
            // the rename (materializes a trailing hole under `-S`).
            if let Some(mut writer) = sparse_writer {
                writer.finish()?;
            }
            staged.commit(cfg.fsync, cfg.backup)?;
            if let Some(meta) = &meta {
                apply_source_meta_sync(&path, meta, cfg)?;
                if cfg.verify {
                    let stat = std::fs::metadata(&path).map_err(Error::Io)?;
                    if !dest_still_matches(&stat, meta) {
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        })
        .await
        .map_err(|e| Error::Other(format!("Commit task panicked: {e}")))??;
        if !durable {
            tracing::warn!("destination changed after apply: {rel}; source not removed");
            state.skipped.push(SkippedFile::new(rel.clone(), DEST_CHANGED_MSG));
            return Ok(());
        }
        state.renamed_dirs.insert(parent);
        state.files_received += 1;
        state.bytes_transferred += written;
        if let Some(hash) = hash {
            state.hashes.insert(rel, hash);
        }
        Ok(())
    }

    /// Respond to a `SignatureRequest` with basis signatures for the requested
    /// paths (on demand, only what the sender needs). Signatures are generated
    /// concurrently, bounded by the apply window: each one is a full file
    /// read + chunk + hash pass, and the sender waits for the whole response,
    /// so serial generation would dominate an update-heavy sync.
    async fn handle_signature_request<W: AsyncWrite + Unpin>(
        &self,
        ctrl_send: &mut W,
        paths: Vec<String>,
    ) -> Result<()> {
        let jobs = self.apply_jobs.max(1);
        let rollsum = self.rollsum;
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(jobs));
        let mut tasks = Vec::with_capacity(paths.len());
        for rel in paths {
            // Acquire before spawning: at most `jobs` signature tasks are in
            // flight, so a huge request does not pile up blocking threads.
            let permit = semaphore.clone().acquire_owned().await;
            let path = self.safe_join(&rel)?;
            tasks.push(tokio::spawn(async move {
                let _permit = permit;
                (rel, signature_for_path(path, rollsum).await)
            }));
        }
        let mut signatures: Vec<SignatureEntry> = Vec::with_capacity(tasks.len());
        for task in tasks {
            let (rel, sig) = task
                .await
                .map_err(|e| Error::Other(format!("Signature task panicked: {e}")))?;
            if let Ok(sig) = sig {
                signatures.push(SignatureEntry {
                    file_path: rel,
                    signature: sig,
                });
            }
        }
        stream::send_frame(ctrl_send, &Frame::SignatureResponse { signatures }).await?;
        Ok(())
    }

    /// Create directory chains announced by the sender (`MakeDir`), applying
    /// their source metadata after the chain is in place.
    async fn handle_make_dir(&self, state: &mut ApplyState, paths: Vec<String>) -> Result<()> {
        for rel in paths {
            let path = self.safe_join(&rel)?;
            // Removes any file blocking the path (file → directory
            // replacement) and creates the chain. A failure (locked file,
            // permissions) skips this directory; its files are then skipped
            // individually when they fail to stage.
            let dir = path.clone();
            let made =
                tokio::task::spawn_blocking(move || crate::platform::staging::make_dir_chain(&dir))
                    .await
                    .map_err(|e| Error::Other(format!("MakeDir task panicked: {e}")))?;
            match made {
                Ok(()) => {
                    if let Some(meta) = state.meta_map.get(&rel) {
                        state.dir_metas.push((path.clone(), Arc::clone(meta)));
                    }
                    state
                        .renamed_dirs
                        .insert(path.parent().map(Path::to_path_buf).unwrap_or(path));
                    state.files_received += 1;
                }
                Err(e) => {
                    tracing::warn!("skipping {}: {e}", path.display());
                    state.skipped.push(SkippedFile::new(rel.clone(), e.to_string()));
                }
            }
        }
        Ok(())
    }

    /// Create symlinks, `.lnk` shortcuts, and hard links announced by the
    /// sender (`CreateLinks`). Hard link targets must already be on disk, so
    /// in-flight file applies are joined first. Failures (missing hard link
    /// target, a `.lnk` that cannot be generated) skip the link instead of
    /// aborting the sync. The receiver makes no decisions here: the sender
    /// already picked the kind and the target (spec §3.2).
    async fn handle_create_links(
        &self,
        state: &mut ApplyState,
        links: Vec<LinkSpec>,
        hardlinks: Vec<crate::protocol::HardlinkSpec>,
        specials: Vec<crate::protocol::SpecialSpec>,
    ) -> Result<()> {
        drain_applies(state).await?;
        let cfg = self.apply_cfg();
        for spec in links {
            let path = self.safe_join(&spec.path)?;
            let display_path = path.display().to_string();
            // The source manifest carries the link's own mtime; restore it
            // after creation (Unix only — see `restore_symlink_meta`).
            let meta = state.meta_map.get(&spec.path).cloned();
            let made = tokio::task::spawn_blocking(move || match spec.kind {
                LinkKind::Symlink => {
                    create_symlink(&path, &spec.target)?;
                    restore_link_meta(&path, meta.as_deref(), cfg, true)
                }
                LinkKind::Lnk => {
                    create_lnk(&path, &spec.target)?;
                    restore_link_meta(&path, meta.as_deref(), cfg, false)
                }
            })
            .await
            .map_err(|e| Error::Other(format!("CreateLinks task panicked: {e}")))?;
            match made {
                Ok(()) => state.files_received += 1,
                Err(e) => {
                    tracing::warn!("skipping link {display_path}: {e}");
                    state.skipped.push(SkippedFile::new(spec.path, e.to_string()));
                }
            }
        }
        // Specials (fifos/sockets/devices): create the node, then apply
        // mode/mtime from the manifest. A device `mknod` without root is
        // `EPERM` and skips the entry (rsync behavior); `mkfifo` needs no
        // privileges.
        for spec in specials {
            let rel = spec.path.clone();
            let kind = spec.kind;
            let rdev = spec.rdev;
            let path = self.safe_join(&rel)?;
            let display_path = path.display().to_string();
            let meta = state.meta_map.get(&rel).cloned();
            let made = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                crate::platform::fs::create_special(
                    &path,
                    kind,
                    rdev,
                    meta.as_ref().map_or(0o644, |m| m.mode),
                )?;
                if let Some(meta) = &meta {
                    apply_source_meta_sync(&path, meta, cfg)?;
                }
                Ok(())
            })
            .await
            .map_err(|e| Error::Other(format!("CreateLinks task panicked: {e}")))?;
            match made {
                Ok(()) => state.files_received += 1,
                Err(e) => {
                    tracing::warn!("skipping special {display_path}: {e}");
                    state.skipped.push(SkippedFile::new(rel.clone(), e.to_string()));
                }
            }
        }
        for spec in hardlinks {
            let rel = spec.path.clone();
            let target = spec.target.clone();
            let path = self.safe_join(&rel)?;
            let target_path = self.safe_join(&target)?;
            let display_path = path.display().to_string();
            let made = tokio::task::spawn_blocking(move || create_hardlink(&path, &target_path))
                .await
                .map_err(|e| Error::Other(format!("CreateLinks task panicked: {e}")))?;
            match made {
                Ok(()) => state.files_received += 1,
                Err(e) => {
                    tracing::warn!("skipping hard link {display_path}: {e}");
                    state.skipped.push(SkippedFile::new(rel.clone(), e.to_string()));
                }
            }
        }
        Ok(())
    }

    /// Apply a `DeleteRequest`: remove each path, enforcing `--max-delete`
    /// as a whole-batch safety valve and skipping per-file failures.
    ///
    /// In-flight file applies are joined first (like `CreateLinks`): a
    /// deletion sees a settled tree, so `prune_empty_dirs` cannot observe a
    /// temporarily empty root while the last real file is still being
    /// committed asynchronously — which would otherwise remove the root
    /// itself.
    async fn handle_delete_request(
        &self,
        state: &mut ApplyState,
        paths: Vec<String>,
    ) -> Result<()> {
        if let Some(max) = self.max_delete
            && paths.len() as u64 > max
        {
            return Err(Error::Other(format!(
                "Refusing to delete {} files: limit is {max} (--max-delete)",
                paths.len()
            )));
        }
        drain_applies(state).await?;
        for rel in paths {
            let path = self.safe_join(&rel)?;
            match tokio::fs::symlink_metadata(&path).await {
                Ok(meta) if meta.is_dir() => {
                    // Best-effort: only removes empty dirs; non-empty ones are
                    // left (their files should already have been deleted).
                    let _ = tokio::fs::remove_dir(&path).await;
                }
                Ok(_) => {
                    let p = path.clone();
                    let backup = self.backup;
                    let removed = tokio::task::spawn_blocking(move || {
                        if backup {
                            // rsync `--backup` also backs up files that are
                            // about to be deleted, not only replaced ones.
                            crate::platform::staging::backup_existing(&p)
                        } else {
                            crate::platform::fs::remove_file_any(&p)
                        }
                    })
                    .await
                    .map_err(|e| Error::Other(format!("Delete task panicked: {e}")))?;
                    match removed {
                        Ok(()) => {
                            // Prune now-empty parent dirs, bottom-up.
                            prune_empty_dirs(path.parent(), &self.root).await;
                        }
                        Err(e) => {
                            tracing::warn!("skipping delete {}: {e}", path.display());
                            state.skipped.push(SkippedFile::new(rel.clone(), e.to_string()));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::Io(e)),
            }
        }
        Ok(())
    }

    /// Acknowledge the peer's `Done` frame after draining in-flight applies.
    /// Returns `true` to end the receive loop.
    async fn handle_done<W: AsyncWrite + Unpin>(
        &self,
        state: &mut ApplyState,
        ctrl_send: &mut W,
        files: u64,
        bytes: u64,
    ) -> Result<bool> {
        tracing::info!("peer done: {files} files, {bytes} bytes");
        drain_applies(state).await?;
        // Make the renames durable *before* acknowledging: the sender may
        // delete the source immediately after the Ack, so a crash must not be
        // able to lose a just-renamed destination file.
        sync_dirs(&state.renamed_dirs).await;
        // Acknowledge so the sender can close the connection without
        // discarding unread stream data. The hashes are consumed (the
        // receive loop ends here), so they are taken rather than cloned.
        stream::send_frame(
            ctrl_send,
            &Frame::Ack {
                files: state.files_received,
                bytes: state.bytes_transferred,
                skipped: state.skipped.clone(),
                hashes: std::mem::take(&mut state.hashes).into_iter().collect(),
            },
        )
        .await?;
        Ok(true)
    }

    /// Join a peer-supplied relative path under the receiver root.
    fn safe_join(&self, rel: &str) -> Result<PathBuf> {
        self.sanitizer
            .join(rel)
            .map_err(|e| Error::Other(format!("Unsafe path '{rel}': {e}")))
    }
}

/// Compute a basis signature for `path` on a blocking thread, so the file
/// I/O and hashing never stall the async reactor. Free-standing so parallel
/// signature tasks can spawn it without borrowing the receiver.
async fn signature_for_path(path: PathBuf, rollsum: bool) -> std::io::Result<Signature> {
    tokio::task::spawn_blocking(move || {
        // Cache hit: the destination's signature was stored by the transfer
        // that produced it, keyed by its size+mtime (the same trust as the
        // quick check). Saves the full read + chunk + hash of the basis.
        if let Some(sig) = std::fs::metadata(&path).ok().and_then(|meta| {
            crate::sync::sigcache::lookup(
                &path,
                meta.len(),
                crate::platform::fs::mtime_secs(&meta),
                crate::platform::fs::mtime_nsecs(&meta),
            )
        }) {
            return Ok(sig);
        }
        // Missing or directory destination: no basis (empty signature).
        match crate::platform::fs::open_basis(&path) {
            Ok(Some(mut file)) => {
                let meta = file.metadata().map_err(std::io::Error::other)?;
                if rollsum {
                    let block = crate::delta::rollsum::block_size(meta.len());
                    Signature::generate_rollsum(&mut file, block)
                        .map_err(|e| std::io::Error::other(e.to_string()))
                } else {
                    Signature::generate(&mut file)
                        .map_err(|e| std::io::Error::other(e.to_string()))
                }
            }
            Ok(None) => Ok(Signature::new(0)),
            Err(e) => Err(e),
        }
    })
    .await
    .map_err(|e| std::io::Error::other(format!("Signature task panicked: {e}")))?
}

/// Mutable state of the recipe-apply loop, threaded through the per-frame
/// handlers so each frame arm stays a small method.
struct ApplyState {
    bytes_transferred: u64,
    files_received: u64,
    meta_map: HashMap<String, Arc<FileMeta>>,
    apply_semaphore: Arc<tokio::sync::Semaphore>,
    dir_metas: Vec<(PathBuf, Arc<FileMeta>)>,
    renamed_dirs: HashSet<PathBuf>,
    skipped: Vec<SkippedFile>,
    /// Wire path → BLAKE3 hash of the applied bytes (populated when
    /// verification was requested; returned to the sender in the `Ack`).
    hashes: HashMap<String, [u8; 32]>,
    /// Wire path → apply task of files applied this run (cross-file deltas
    /// join their basis's apply before reading it).
    applied_this_run: HashMap<String, tokio::task::JoinHandle<Result<ApplyOutcome>>>,
    /// The one blocking task applying the current `Batch` frame (all its
    /// files in a single pass — the per-file task overhead dominates
    /// small-file trees). Joined by `drain_applies` like the per-file
    /// handles; batch files are never cross-file bases (≤ 2 MiB), so they
    /// need no `applied_this_run` entries.
    batch_apply: Option<tokio::task::JoinHandle<Result<Vec<ApplyOutcome>>>>,
    in_flight: Option<ChunkedFile>,
}

impl ApplyState {
    fn new(source_meta: Vec<FileMeta>, apply_jobs: usize) -> Self {
        let meta_map: HashMap<String, Arc<FileMeta>> = source_meta
            .into_iter()
            .map(|m| (m.path.clone(), Arc::new(m)))
            .collect();
        // Bounded in-flight applies so disk writes overlap across files without
        // unbounded buffering; joined before the final Ack (or drained on error).
        let apply_semaphore = Arc::new(tokio::sync::Semaphore::new(apply_jobs));
        Self {
            bytes_transferred: 0,
            files_received: 0,
            meta_map,
            apply_semaphore,
            dir_metas: Vec::new(),
            renamed_dirs: HashSet::new(),
            skipped: Vec::new(),
            hashes: HashMap::new(),
            applied_this_run: HashMap::new(),
            batch_apply: None,
            in_flight: None,
        }
    }
}

/// Loop control returned by the frame handlers.
/// Join all in-flight file-application tasks, folding their outcomes into the
/// running counters. Called at `Done` (before the Ack) and before
/// `CreateLinks` (hard link targets must already be on disk). The `Batch`
/// frame's single apply task is joined here too.
async fn drain_applies(state: &mut ApplyState) -> Result<()> {
    if let Some(handle) = state.batch_apply.take() {
        for outcome in handle
            .await
            .map_err(|e| Error::Other(format!("Batch apply task panicked: {e}")))?
            ?
        {
            fold_outcome(state, outcome);
        }
    }
    let handles: Vec<_> = state.applied_this_run.drain().map(|(_, h)| h).collect();
    for handle in handles {
        fold_outcome(
            state,
            handle
                .await
                .map_err(|e| Error::Other(format!("Apply task panicked: {e}")))??,
        );
    }
    Ok(())
}

/// Fold one file-application outcome into the running counters.
fn fold_outcome(state: &mut ApplyState, outcome: ApplyOutcome) {
    match outcome {
        ApplyOutcome::Applied(sent, parent, hash) => {
            state.bytes_transferred += sent;
            state.files_received += 1;
            state.renamed_dirs.insert(parent);
            if let Some((rel, hash)) = hash {
                state.hashes.insert(rel, hash);
            }
        }
        ApplyOutcome::Skipped(skip_path, reason) => {
            tracing::warn!("skipping {skip_path}: {reason}");
            state.skipped.push(SkippedFile::new(skip_path, reason));
        }
    }
}

/// Outcome of applying one file recipe on the receiver.
enum ApplyOutcome {
    /// File applied: bytes written, parent dir for the rename-sync pass, and
    /// (verification mode) the wire path + BLAKE3 hash of the applied bytes.
    Applied(u64, PathBuf, Option<(String, [u8; 32])>),
    /// File skipped due to a per-file condition (locked, path too long, ...).
    /// The wire-relative path is recorded so the sender can match skips
    /// against its transfer list for `--remove-source-files`.
    Skipped(String, String),
}

/// A deferred chunk write: a blocking task that writes one chunk and returns
/// the file handle plus the verification hasher (see `ChunkedFile`).
type ChunkWrite = tokio::task::JoinHandle<std::io::Result<(SparseWriter, Option<blake3::Hasher>)>>;

/// The in-flight state of a sequential chunked file transfer
/// ([`Frame::FileStart`] → [`Frame::FileChunk`]* → [`Frame::FileEnd`]).
struct ChunkedFile {
    /// Stable identifier from the announcing `FileStart`; chunks are matched
    /// against it (protocol v12).
    file_id: u64,
    /// The staged sink; `None` when staging failed (chunks are drained and
    /// the file reported skipped at `FileEnd`).
    staged: Option<StagedFile>,
    /// Open write handle into the staged file — the sparse filter when
    /// `-S` is on, a plain passthrough otherwise (one type either way). While
    /// a chunk write is deferred, the handle lives inside the task.
    file: Option<SparseWriter>,
    /// The deferred write of the most recently arrived chunk — the head of
    /// the write chain (each task blocks on the previous one's join, so
    /// writes are strictly sequential); joined at `FileEnd` and on the abort
    /// path. Returns the file handle and the verification hasher.
    pending_write: Option<ChunkWrite>,
    /// Bounds the write chain (chunks queued in memory).
    write_slots: std::sync::Arc<tokio::sync::Semaphore>,
    /// Bytes written so far.
    offset: u64,
    /// Destination path.
    file_path: PathBuf,
    /// Source-relative path for display/progress.
    display: String,
    /// Incremental BLAKE3 of the bytes written (verification mode only).
    hasher: Option<blake3::Hasher>,
    /// Source metadata to apply after commit.
    meta: Option<Arc<FileMeta>>,
    /// Full size of the file.
    size: u64,
    /// Staging failed: drain without writing, skip at `FileEnd`.
    failed: bool,
}

/// Per-file apply flags, captured once from the receiver and threaded
/// through the apply tasks — the bundle the wire contract needs (the
/// alternative is the same 8 `self` fields copied at every apply site).
#[derive(Clone, Copy, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the apply flags mirror the CLI one-to-one; bundled once instead of at every site"
)]
struct ApplyCfg {
    fsync: bool,
    backup: bool,
    verify: bool,
    preserve_times: bool,
    archive: bool,
    sparse: bool,
    preserve_atimes: bool,
    xattrs: bool,
}

impl Receiver {
    fn apply_cfg(&self) -> ApplyCfg {
        ApplyCfg {
            fsync: self.fsync,
            backup: self.backup,
            verify: self.verify,
            preserve_times: self.preserve_times,
            archive: self.archive,
            sparse: self.sparse,
            preserve_atimes: self.preserve_atimes,
            xattrs: self.xattrs,
        }
    }
}

/// Apply a delta recipe to the destination atomically: stage, patch (with
/// checksum verification), commit, and apply source metadata — all in one
/// blocking task so file I/O never stalls the async reactor.
///
/// Per-file conditions (another process holding the destination open, a path
/// too long for the platform, a reserved name, ...) skip the file instead of
/// aborting the whole sync. The argument count is the price of the wire
/// contract; the established `#[expect]` pattern (see `FileMeta::new`) applies.
#[expect(clippy::too_many_arguments)]
async fn apply_recipe(
    path: PathBuf,
    delta: Delta,
    source_signature: Option<Signature>,
    basis_path: Option<PathBuf>,
    meta: Option<Arc<FileMeta>>,
    cfg: ApplyCfg,
    display_path: String,
    progress: Option<ProgressFn>,
    files_total: u64,
) -> Result<ApplyOutcome> {
    tokio::task::spawn_blocking(move || {
        apply_recipe_blocking(
            path,
            delta,
            source_signature,
            basis_path,
            meta,
            cfg,
            display_path,
            progress,
            files_total,
        )
    })
    .await
    .map_err(|e| Error::Other(format!("Apply task panicked: {e}")))
}

/// The synchronous core of one file application: stage, patch (with
/// checksum verification), commit, and apply source metadata. Free-standing
/// so both the single-recipe task and the batch task (one blocking task per
/// `Batch` frame — the per-file task overhead dominates small-file trees)
/// share it. The `progress` callback is the per-file reporter; per-file
/// conditions skip the file instead of aborting the sync.
#[expect(clippy::too_many_arguments)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the values arrive owned across the spawn_blocking task boundary"
)]
fn apply_recipe_blocking(
    path: PathBuf,
    delta: Delta,
    source_signature: Option<Signature>,
    basis_path: Option<PathBuf>,
    meta: Option<Arc<FileMeta>>,
    cfg: ApplyCfg,
    display_path: String,
    progress: Option<ProgressFn>,
    files_total: u64,
) -> ApplyOutcome {
    let outcome: Result<ApplyOutcome> = (|| {
        // A missing or directory destination has no delta basis (the file
        // replaces a directory atomically at commit). A cross-file delta
        // reads its basis from the announced other file instead. A zero
        // basis (a fresh `Copy` — the whole small-file batch) skips the
        // open entirely: the destination is known absent or irrelevant.
        let basis = if delta.basis_size > 0 {
            match crate::platform::fs::open_basis(basis_path.as_deref().unwrap_or(&path)) {
                Ok(basis) => basis,
                Err(e) => return Err(e.into()),
            }
        } else {
            None
        };

            let staged = StagedFile::new(&path)?;
            staged.prepare(delta.source_size, cfg.sparse)?;

            // One shared hasher covers the delta checksum verification (when
            // the sender computed one) and the per-file verification report —
            // and only then: in the default mode nothing needs a digest, so
            // no whole-file BLAKE3 pass happens at all.
            let total = delta.source_size;
            let need_hash = delta.checksum.is_some() || cfg.verify;
            let mut hasher = need_hash.then(blake3::Hasher::new);
            let hash = {
                // The sparse filter sits under the hasher: it sees every
                // logical byte (turning zero runs into holes changes nothing
                // the hasher observes), and it owns the file for the final
                // truncate.
                let mut sparse_writer = SparseWriter::new(staged.open()?, total, cfg.sparse);
                let hash = if let Some(data) = whole_literal(&delta) {
                    // The zero-basis whole-content fast path: the small-file
                    // batch's every entry is exactly this (one Literal, no
                    // basis) — write the bytes directly instead of routing
                    // them through the generic op interpreter and an empty
                    // basis cursor.
                    if let Some(h) = &mut hasher {
                        h.update(data);
                    }
                    match &progress {
                        Some(report) => {
                            let mut stream = crate::sync::executor::ProgressStream::new(
                                &mut sparse_writer,
                                display_path.clone(),
                                total,
                                files_total,
                                Arc::clone(report),
                            );
                            std::io::Write::write_all(&mut stream, data).map_err(Error::Io)?;
                        }
                        None => sparse_writer.write_all(data).map_err(Error::Io)?,
                    }
                    match hasher {
                        Some(h) => {
                            let d = *h.finalize().as_bytes();
                            if let Some(expected) = delta.checksum
                                && d != expected
                            {
                                return Err(crate::Error::Delta(
                                    crate::delta::error::DeltaError::ChecksumMismatch {
                                        expected,
                                        actual: d,
                                    },
                                ));
                            }
                            d
                        }
                        None => [0u8; 32],
                    }
                } else {
                    match &progress {
                        Some(report) => {
                            let mut stream = crate::sync::executor::ProgressStream::new(
                                &mut sparse_writer,
                                display_path.clone(),
                                total,
                                files_total,
                                Arc::clone(report),
                            );
                            apply_chain(&mut stream, basis, &delta, hasher.as_mut())?
                        }
                        None => apply_chain(&mut sparse_writer, basis, &delta, hasher.as_mut())?,
                    }
                };
                if let Some(report) = &progress {
                    report(&display_path, total, total, files_total);
                }
                // Flush pending zeros and truncate to the announced size
                // (materializes a trailing hole under `-S`) before commit.
                sparse_writer.finish()?;
                hash
            };

            // Verification implies durability: the sender will delete the
            // source on our word, so the file must be on disk first.
            staged.commit(cfg.fsync || cfg.verify, cfg.backup)?;
            if let Some(meta) = &meta {
                apply_source_meta_sync(&path, meta, cfg)?;
                // The destination must still be exactly what we applied — a
                // post-apply modification (another process touching the
                // storage copy) voids the hash we are about to report.
                if cfg.verify {
                    let stat = std::fs::metadata(&path).map_err(Error::Io)?;
                    if !dest_still_matches(&stat, meta) {
                        return Err(Error::Other(DEST_CHANGED_MSG.to_string()));
                    }
                }
            }
            // Cache the new destination content's basis signature (the
            // sender's free byproduct) so the next run's basis signing can
            // skip re-reading the file. Keyed by the applied size+mtime;
            // with `--no-times` the applied mtime differs from the source's,
            // so the entry would never match — skip it.
            if let (Some(sig), Some(meta)) = (source_signature, meta.as_ref())
                && cfg.preserve_times
            {
                crate::sync::sigcache::store(
                    &path,
                    delta.source_size,
                    meta.mtime,
                    meta.mtime_nsec,
                    &sig,
                );
            }
            let parent = path
                .parent()
                .map_or_else(|| path.clone(), Path::to_path_buf);
            let verified = cfg.verify.then_some((display_path.clone(), hash));
            Ok(ApplyOutcome::Applied(delta.source_size, parent, verified))
        })();
        // Per-file conditions (locked destination, path too long, ...) skip
        // the file instead of aborting the whole sync; `display_path` (the
        // wire path) moves into the skip record so the sender can match it.
        match outcome {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!("skipping {display_path}: {e}");
                ApplyOutcome::Skipped(display_path, e.to_string())
            }
        }
}

/// If the delta is exactly one literal covering the whole source (the
/// small-file batch's every entry: no basis, a single Literal op), return
/// its data — the direct-write fast path skips the generic op interpreter.
fn whole_literal(delta: &Delta) -> Option<&[u8]> {
    if delta.basis_size != 0 || delta.ops.len() != 1 {
        return None;
    }
    match delta.ops.first() {
        Some(crate::delta::DeltaOp::Literal(data)) => Some(data),
        _ => None,
    }
}

/// Apply source metadata (mode + mtime, atime under `-U`, xattrs under `-X`)
/// synchronously. The mode is the sender-computed final value (spec §2.2
/// matrix) — always applied on Unix, a no-op elsewhere; times can be opted
/// out (`--no-times`). Owner/group are never applied here (0-Root).
fn apply_source_meta_sync(path: &Path, meta: &FileMeta, cfg: ApplyCfg) -> std::io::Result<()> {
    let (uid, gid) = (meta.uid, meta.gid);
    let pmeta = platform_meta(meta, meta.mode);
    crate::platform::fs::apply_meta(path, &pmeta, false, true, cfg.preserve_times, cfg.preserve_atimes)?;
    if cfg.archive
        && let Err(e) = crate::platform::fs::apply_owner(path, uid, gid, false)
    {
        // Best-effort (`-a` only): a non-root receiver keeps the SSH user's
        // ownership — the default 0-Root model. Never fails the file.
        tracing::warn!("chown {}: {e}", path.display());
    }
    if cfg.xattrs
        && let Some(xattr_list) = &meta.xattrs
        && !xattr_list.is_empty()
        && let Err(e) = crate::platform::fs::apply_xattrs(path, xattr_list)
    {
        // Best-effort (`-X` only): an unsettable attribute (a `security.*`
        // name as a non-root receiver, a read-only filesystem, ...) warns and
        // keeps the file.
        tracing::warn!("xattrs {}: {e}", path.display());
    }
    Ok(())
}

/// The platform-layer metadata for applying a wire `FileMeta`: the wire mtime
/// and atime in whole seconds, and the given mode (`0` = do not apply
/// permission bits, e.g. symlinks).
fn platform_meta(meta: &FileMeta, mode: u32) -> crate::platform::fs::FileMeta {
    crate::platform::fs::FileMeta {
        size: meta.size,
        mode,
        mtime_sec: crate::sync::wire::mtime_from_wire(meta.mtime),
        // The nanosecond remainder is always restored — the quick check
        // compares it unconditionally.
        mtime_nsec: meta.mtime_nsec,
        atime_sec: crate::sync::wire::mtime_from_wire(meta.atime),
        atime_nsec: meta.atime_nsec,
    }
}

/// Restore a created symlink's own metadata from the source manifest after
/// creation, without touching the link target.
///
/// Restore a created link's own mtime (and, under `-a`, ownership) from the
/// source manifest. A symlink needs the `AT_SYMLINK_NOFOLLOW` times path and
/// exists only on Unix; a `.lnk` shortcut is a regular file, so the plain
/// metadata path works on every platform — the `symlink` flag picks the
/// platform-appropriate variant.
fn restore_link_meta(
    path: &Path,
    meta: Option<&FileMeta>,
    cfg: ApplyCfg,
    symlink: bool,
) -> std::io::Result<()> {
    #[cfg(not(unix))]
    if symlink {
        // A POSIX symlink cannot exist off Unix (the sender never emits one
        // for a Windows target) — the no-op keeps one restore path.
        let _ = (path, meta, cfg);
        return Ok(());
    }
    if let Some(meta) = meta {
        let (uid, gid) = (meta.uid, meta.gid);
        let meta = platform_meta(meta, 0);
        crate::platform::fs::apply_meta(path, &meta, symlink, false, cfg.preserve_times, cfg.preserve_atimes)?;
        if cfg.archive
            && let Err(e) = crate::platform::fs::apply_owner(path, uid, gid, symlink)
        {
            tracing::warn!("chown {}: {e}", path.display());
        }
    }
    Ok(())
}

/// Create a symbolic link at `path` pointing to the literal `target`,
/// replacing whatever currently occupies `path` (file, symlink, or
/// directory). Unix-only: the sender never emits a `Symlink` instruction for
/// a Windows target (it sends `.lnk` or content instead, spec §3.2), so a
/// `Symlink` reaching a non-Unix receiver is a protocol anomaly — the link is
/// skipped and reported rather than silently downgraded.
fn create_symlink(path: &Path, target: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        create_then_install(path, |tmp| std::os::unix::fs::symlink(target, tmp))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, target);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlink creation is unsupported on this target; the sender should have sent a .lnk or content",
        ))
    }
}

/// Create a Windows `.lnk` shortcut at `path` whose target is the rewritten,
/// DEST-relative `target` (backslash-separated). The receiver only executes:
/// it builds a minimal Shell Link carrying the relative path in its
/// `StringData` (the `lnk` crate's writer emits header + `StringData`).
/// Best-effort per spec §3.2 — a Windows Explorer resolution depends on the
/// destination filesystem layout, which mirrors the source by construction.
fn create_lnk(path: &Path, target: &str) -> std::io::Result<()> {
    // The wire target is '/'-separated; a .lnk uses '\'. Force UTF-16 so a
    // non-ASCII relative target is not mangled by the code-page fallback.
    let win_target = target.replace('/', "\\");
    create_then_install(path, |tmp| {
        let mut link = lnk::ShellLink::default().with_encoding(&lnk::StringEncoding::Unicode);
        link.set_relative_path(Some(win_target.clone()));
        link.save(tmp)
            .map_err(|e| std::io::Error::other(e.to_string()))
    })
}

/// Create a hard link at `path` to the already-transferred file at `target`
/// (both root-relative), replacing whatever currently occupies `path`.
fn create_hardlink(path: &Path, target: &Path) -> std::io::Result<()> {
    create_then_install(path, |tmp| std::fs::hard_link(target, tmp))
}

/// Create a filesystem object (symlink, `.lnk` file, hard link) at `path`
/// without a remove-then-create gap: `make` creates the object at a unique
/// sibling temp name, which is then atomically renamed over `path`. A
/// directory currently occupying `path` is moved aside first (rename cannot
/// replace a directory) and removed only after the install succeeds; on any
/// failure the previous occupant is restored and the temp cleaned up by the
/// [`StagedFile`] drop.
fn create_then_install<F>(path: &Path, make: F) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    let staged = StagedFile::new(path)?;
    make(staged.path())?;
    staged.commit(false, false)
}

/// Sync the directories that received a commit, making the completed renames
/// durable without a per-file fsync. Best effort per platform — see
/// [`crate::platform::fs::sync_dir`].
async fn sync_dirs(dirs: &HashSet<PathBuf>) {
    if dirs.is_empty() {
        return;
    }
    let dirs: Vec<PathBuf> = dirs.iter().cloned().collect();
    let _ = tokio::task::spawn_blocking(move || {
        for dir in &dirs {
            let _ = crate::platform::fs::sync_dir(dir);
        }
    })
    .await;
}

async fn prune_empty_dirs(dir: Option<&Path>, root: &Path) {
    let mut current = dir.map(Path::to_path_buf);
    while let Some(path) = current {
        if path == root {
            break;
        }
        let Ok(mut entries) = tokio::fs::read_dir(&path).await else {
            return;
        };
        if entries.next_entry().await.ok().flatten().is_some() {
            // Not empty; stop pruning upward.
            return;
        }
        let _ = tokio::fs::remove_dir(&path).await;
        current = path.parent().map(Path::to_path_buf);
    }
}

/// Apply a delta, feeding every logical byte through the shared hasher when
/// one is provided.
///
/// `hasher` is `Some` only when a digest is needed — the delta checksum
/// verification (the sender computed one) or the per-file verification
/// report — so the default mode applies without any whole-file BLAKE3 pass.
/// Returns the final digest, or zeros when no hasher was given.
fn apply_chain<W: std::io::Write>(
    out: W,
    basis: Option<std::fs::File>,
    delta: &Delta,
    mut hasher: Option<&mut blake3::Hasher>,
) -> crate::Result<[u8; 32]> {
    match basis {
        Some(mut basis) => apply_patch(&mut basis, delta, out, hasher.as_deref_mut())?,
        None => apply_patch(std::io::Cursor::new(&[][..]), delta, out, hasher.as_deref_mut())?,
    }
    Ok(match hasher {
        Some(h) => *h.finalize().as_bytes(),
        None => [0u8; 32],
    })
}

/// Zero-run detection threshold for [`SparseWriter`]: runs of zeros at least
/// this long become holes (lseek) instead of allocated blocks.
const SPARSE_RUN_MIN: usize = 4096;

/// Maximum chunked writes queued in memory (see `handle_file_chunk`): the
/// loop never joins mid-stream, so a slow disk cannot stall the wire, and the
/// queue depth bounds the buffered bytes (1 MiB chunks — ~32 MiB).
const WRITE_QUEUE_DEPTH: usize = 32;

/// A write filter that turns runs of zeros into holes (`--sparse`, rsync
/// `-S`): each write is split at its non-zero bytes, and once a pending zero
/// run reaches [`SPARSE_RUN_MIN`] bytes the writer seeks past it instead of
/// writing — so the destination stays sparse (VM images, database files)
/// even when the zero run arrives inside a larger mixed buffer (a whole-file
/// delta literal). The logical byte stream is unchanged — hashers above this
/// writer still see every byte, and `finish()` truncates to the announced
/// size, materializing a trailing hole. With the feature off it is a
/// transparent passthrough, so both write paths use one type.
struct SparseWriter {
    inner: std::fs::File,
    /// Whether hole detection is active (`-S`).
    enabled: bool,
    /// Final logical size; `finish()` truncates to it.
    total: u64,
    /// Pending zero bytes below the threshold — written out on the first
    /// non-zero byte (they are real file bytes) or at `finish()`. A *count*,
    /// not a buffer: a single write may carry a huge all-zero run, and only
    /// the sub-threshold remainder is ever materialized, so memory stays
    /// bounded by [`SPARSE_RUN_MIN`].
    zeros_pending: usize,
}

impl SparseWriter {
    /// Wrap `inner`, writing a file of `total` logical bytes; `enabled`
    /// gates hole detection.
    fn new(inner: std::fs::File, total: u64, enabled: bool) -> Self {
        Self {
            inner,
            enabled,
            total,
            zeros_pending: 0,
        }
    }

    /// Flush the pending zero run (allocated — below the threshold) and
    /// truncate to the final logical size.
    fn finish(&mut self) -> std::io::Result<()> {
        self.flush_zeros()?;
        self.inner.set_len(self.total)
    }

    /// Truncate to `size` (abort path: the partial file is cut to the bytes
    /// actually written so the next run's quick check detects it).
    fn set_len(&mut self, size: u64) -> std::io::Result<()> {
        self.zeros_pending = 0;
        self.inner.set_len(size)
    }

    fn flush_zeros(&mut self) -> std::io::Result<()> {
        if self.zeros_pending > 0 {
            // Only the sub-threshold remainder is materialized — at most
            // `SPARSE_RUN_MIN` bytes.
            self.inner.write_all(&vec![0u8; self.zeros_pending])?;
            self.zeros_pending = 0;
        }
        Ok(())
    }

    /// Accumulate a zero run of `n` bytes: below the threshold they stay
    /// pending (written later as real bytes); once the count passes it, the
    /// whole pending run is skipped with a seek (a hole). Memory is O(1)
    /// regardless of the run's size.
    fn buffer_zeros(&mut self, n: usize) -> std::io::Result<()> {
        self.zeros_pending += n;
        if self.zeros_pending >= SPARSE_RUN_MIN {
            self.inner
                .seek(std::io::SeekFrom::Current(i64::try_from(self.zeros_pending).map_err(
                    |_| std::io::Error::other("zero run exceeds file offset range"),
                )?))?;
            self.zeros_pending = 0;
        }
        Ok(())
    }
}

impl std::io::Write for SparseWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !self.enabled {
            return self.inner.write(buf);
        }
        // Split the buffer at its non-zero bytes: zero runs accumulate and
        // seek once past the threshold; non-zero runs flush the pending
        // zeros (they are real file bytes) and write through.
        let mut i = 0;
        while i < buf.len() {
            if buf[i] == 0 {
                let start = i;
                while i < buf.len() && buf[i] == 0 {
                    i += 1;
                }
                self.buffer_zeros(i - start)?;
            } else {
                let start = i;
                while i < buf.len() && buf[i] != 0 {
                    i += 1;
                }
                self.flush_zeros()?;
                self.inner.write_all(&buf[start..i])?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_zeros()
    }
}

/// Whether the destination file's current size and mtime still match the
/// source metadata the receiver applied (verification mode): a post-apply
/// modification by another process would void the hash the receiver is about
/// to report, so the sender must keep the source.
fn dest_still_matches(stat: &std::fs::Metadata, meta: &FileMeta) -> bool {
    stat.len() == meta.size
        && crate::platform::fs::mtime_secs(stat) == meta.mtime
        && crate::platform::fs::mtime_nsecs(stat) == meta.mtime_nsec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dest_still_matches_compares_size_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("img.fits");
        let meta = FileMeta {
            path: "img.fits".to_string(),
            size: 5,
            mtime: 1_700_000_000,
            mtime_nsec: 0,
            mode: 0o644,
            hash: None,
            kind: crate::protocol::FileKind::File,
            link_target: None,
            inode: None,
            rdev: None,
            uid: None,
            gid: None,
            atime: 0,
            atime_nsec: 0,
            xattrs: None,
        };

        // The receiver applied this exact size and mtime.
        std::fs::write(&file, b"12345").unwrap();
        let set = filetime_set(&file, meta.mtime);
        assert!(set.is_ok(), "{set:?}");
        let stat = std::fs::metadata(&file).unwrap();
        assert!(dest_still_matches(&stat, &meta), "matching meta must pass");

        // Same size, modified content (mtime bumped) → mismatch.
        std::fs::write(&file, b"54321").unwrap();
        let stat = std::fs::metadata(&file).unwrap();
        assert!(
            !dest_still_matches(&stat, &meta),
            "a post-apply modification must fail the check"
        );
    }

    /// Set a file's mtime to a whole-second value (via `utimensat`).
    #[allow(clippy::cast_possible_wrap)]
    fn filetime_set(path: &std::path::Path, mtime: u64) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| std::io::Error::other("nul in path"))?;
            let times = [
                libc::timespec {
                    tv_sec: mtime as libc::time_t,
                    tv_nsec: 0,
                },
                libc::timespec {
                    tv_sec: mtime as libc::time_t,
                    tv_nsec: 0,
                },
            ];
            // SAFETY: `c_path` is a valid NUL-terminated path; `times`
            // points to two valid timespecs (test helper).
            let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mtime);
            Err(std::io::Error::other("unsupported on this platform"))
        }
    }

    /// The `.cp2.` staging temp files left in `dir` (none after a successful
    /// or failed link install).
    fn cp2_temps(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".cp2."))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn create_symlink_replaces_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link.txt");
        std::fs::write(&path, b"old").unwrap();

        create_symlink(&path, "target.txt").unwrap();
        assert!(path.is_symlink());
        assert_eq!(
            std::fs::read_link(&path).unwrap(),
            std::path::Path::new("target.txt")
        );
        assert!(cp2_temps(dir.path()).is_empty(), "no staging temps may remain");
    }

    #[cfg(unix)]
    #[test]
    fn create_symlink_replaces_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thing");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("inner.txt"), b"x").unwrap();

        // A directory blocking the path is moved aside and removed after the
        // new link is installed.
        create_symlink(&path, "target.txt").unwrap();
        assert!(path.is_symlink());
        assert!(cp2_temps(dir.path()).is_empty());
    }

    #[test]
    fn create_then_install_failure_keeps_previous_occupant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thing");
        std::fs::write(&path, b"old").unwrap();

        // A `make` that writes the temp but then fails must leave the previous
        // file untouched — and no temp behind (the StagedFile drop cleans up).
        let err = create_then_install(&path, |tmp| {
            std::fs::write(tmp, b"partial").unwrap();
            Err(std::io::Error::other("boom"))
        });
        assert!(err.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert!(cp2_temps(dir.path()).is_empty());
    }

    #[test]
    fn restore_lnk_meta_applies_source_mtime() {
        // A `.lnk` is a regular file, so its own mtime is restored through the
        // plain metadata path (unlike a symlink's AT_SYMLINK_NOFOLLOW).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shortcut.lnk");
        create_lnk(&path, "..\\sub\\target.txt").unwrap();
        let meta = FileMeta {
            path: "shortcut.lnk".to_string(),
            size: 0,
            mtime: 1_600_000_000,
            mtime_nsec: 0,
            mode: 0,
            hash: None,
            kind: crate::protocol::FileKind::Symlink,
            link_target: Some("..\\sub\\target.txt".to_string()),
            inode: None,
            rdev: None,
            uid: None,
            gid: None,
            atime: 0,
            atime_nsec: 0,
            xattrs: None,
        };
        restore_link_meta(
            &path,
            Some(&meta),
            ApplyCfg {
                preserve_times: true,
                ..ApplyCfg::default()
            },
            false,
        )
        .unwrap();
        let mtime = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(mtime, 1_600_000_000);
    }

    #[test]
    fn sparse_writer_turns_long_zero_runs_into_holes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let mut w = SparseWriter::new(file, 1_048_576, true);
        // The whole file in ONE mixed buffer (exactly how a whole-file delta
        // literal arrives): the interior zero run must still become a hole —
        // the writer splits at non-zero bytes instead of only detecting
        // all-zero writes.
        let mut content = vec![0u8; 1_048_576];
        content[..4].copy_from_slice(b"head");
        content[1_048_572..].copy_from_slice(b"tail");
        w.write_all(&content).unwrap();
        w.finish().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 1_048_576, "the logical size is exact");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let blocks = meta.blocks() * 512;
            assert!(
                blocks < 1_048_576 / 4,
                "a 1 MiB hole must not be allocated ({blocks} bytes allocated)"
            );
        }
        let content = std::fs::read(&path).unwrap();
        assert_eq!(&content[..4], b"head");
        assert_eq!(&content[content.len() - 4..], b"tail");
        assert!(content[4..content.len() - 4].iter().all(|&b| b == 0));
    }

    #[test]
    fn sparse_writer_flushes_pending_zeros_before_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let mut w = SparseWriter::new(file, 4096 + 8, true);
        // A below-threshold zero run, then data: the zeros must be written
        // (allocated — they are real bytes), not lost.
        w.write_all(&vec![0u8; 4096]).unwrap();
        w.write_all(b"payload").unwrap();
        w.finish().unwrap();
        let content = std::fs::read(&path).unwrap();
        assert_eq!(content.len(), 4096 + 8);
        assert!(content[..4096].iter().all(|&b| b == 0));
        assert_eq!(&content[4096..4096 + 7], b"payload");
        assert_eq!(content[4096 + 7], 0, "the last byte is finish() padding");
    }

    #[test]
    fn sparse_writer_finish_pads_trailing_hole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tail");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let mut w = SparseWriter::new(file, 8192, true);
        w.write_all(b"abc").unwrap();
        // The tail beyond the writes is a hole; finish() truncates the file
        // to the announced size, materializing it.
        w.finish().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 8192);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert!(
                meta.blocks() * 512 < 8192,
                "the trailing hole must stay unallocated"
            );
        }
    }

    #[test]
    fn sparse_writer_disabled_is_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let mut w = SparseWriter::new(file, 8192, false);
        w.write_all(&vec![0u8; 8192]).unwrap();
        w.finish().unwrap();
        let content = std::fs::read(&path).unwrap();
        assert_eq!(content.len(), 8192);
        assert!(content.iter().all(|&b| b == 0));
    }
}
