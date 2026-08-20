//! Sender role: announce a manifest, plan against the receiver's manifest, and
//! apply the plan — whole-file copies and deltas against the receiver's basis.
//!
//! Adapted from sy's `TaskExecutor` + robosync's `MixedStrategyExecutor`: decision
//! logic is pure (see `planner`/`strategy`), execution is async here.

use std::collections::{HashMap, VecDeque};
use std::io::Read;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::delta::{Delta, DeltaError, Signature, compute_delta_limited, compute_delta_rollsum};
use crate::protocol::{
    BatchFile, BatchItem, FileMeta, Frame, LinkSpec, SignatureEntry, SkippedFile,
    stream,
};
use crate::sync::executor::{ExecutorOptions, ProgressFn};
use crate::sync::filter::FilterSet;
use crate::sync::planner::{Planner, PlannerConfig, SyncAction, SyncPlan, SyncTask};
use crate::sync::scanner::Manifest;
use crate::sync::stats::SyncStats;
use crate::sync::strategy::{FileClass, TransferStrategy, classify_file_size, determine_strategy};
use crate::sync::wire::{
    file_id, file_meta_from_entry, file_source, from_peer, manifest_from_file_meta, wire_rel,
};
use crate::sync::bandwidth::BandwidthLimiter;
use crate::{Error, Result};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Literal payload above which a delta is discarded in favor of the chunked
/// stream. A whole-file `DeltaRecipe` is a single in-memory frame on both
/// sides, and `protocol::stream` rejects payloads over ~1 GiB — so a delta
/// that is mostly literals (no useful basis, or a fully rewritten file) must
/// fall back to the bounded, resumable `FileStart`/`FileChunk`/`FileEnd` path
/// instead of one giant frame.
const DELTA_LITERAL_CHUNK_LIMIT: u64 = 256 * 1024 * 1024;

/// Flush the small-file batch once it holds this many bytes: a tree of
/// all-small files must not accumulate one giant frame (bounded memory, and
/// `protocol::stream` rejects payloads over ~1 GiB — a big enough batch would
/// fail the whole sync).
const BATCH_BYTES_BUDGET: u64 = 128 * 1024 * 1024;

/// Delta paths per signature-request round: bounds the `SignatureResponse`
/// frame (the ~1 GiB wire cap — a sync with many large delta files would
/// otherwise exceed it and fail) and the receiver's serialization peak. The
/// receiver's per-request parallelism is unchanged (each request is still
/// generated with the full apply window).
const SIGNATURE_GROUP: usize = 32;

/// Chunked-stream frame payload. Sized to match the enlarged child pipe
/// capacity (1 MiB — see `platform::fs::enlarge_pipe`): one frame per pipe
/// turn, so per-frame work (hash, serialization) stays interleaved with the
/// transfer instead of batching up and stalling the wire. Bounded memory:
/// two buffers in flight.
const CHUNK_SIZE: usize = 1024 * 1024;

/// The chunked stream's write batch: raw chunk frames are accumulated and
/// written together, so the in-flight data (the batch + the pipe + the
/// socket buffer) keeps a real-network link saturated — a frame-at-a-time
/// write caps at in-flight / RTT (measured ~100 MB/s vs rsync's 650 MB/s
/// over the same link).
const CHUNK_BATCH_BYTES: usize = 8 * 1024 * 1024;

/// Maximum in-flight small-file reads for the batch frame: the reads overlap
/// each other (a serial read loop would starve the wire between reads on a
/// fast link).
const BATCH_READ_WINDOW: usize = 32;

/// The sender half of a sync: applies a [`SyncPlan`] against the receiver.
pub(crate) struct Sender {
    limiter: Option<Arc<BandwidthLimiter>>,
    /// Per-file progress reporter (see `ExecutorOptions::progress`).
    progress: Option<ProgressFn>,
    /// Maximum in-flight delta computations (a sliding window over the
    /// CPU/disk-bound chunk+hash pass). Storage-aware like the receiver's
    /// apply window: 1 on a spinning disk (parallel reads thrash one head),
    /// more on SSD/NVMe; `-j` overrides.
    compute_jobs: usize,
    /// Hash transferred chunked bytes for the post-transfer comparison
    /// (`--verify`/`--remove-source-files`). Off otherwise: the hash is only
    /// consumed by that pass.
    verify_hash: bool,
    /// rsync-style rollsum delta engine (fixed blocks + byte-sliding scan)
    /// instead of `FastCDC`.
    rollsum: bool,
}

impl Sender {
    /// Create a sender, optionally paced by a shared bandwidth limiter and
    /// reporting per-file progress through `progress`. `compute_jobs` bounds
    /// the in-flight delta-computation window (see [`Self::compute_jobs`]);
    /// `verify_hash` enables the chunked-path verification hash.
    pub(crate) fn new(
        bwlimit_bytes: Option<u64>,
        progress: Option<ProgressFn>,
        compute_jobs: usize,
        verify_hash: bool,
        rollsum: bool,
    ) -> Self {
        Self {
            limiter: bwlimit_bytes.map(|b| Arc::new(BandwidthLimiter::new(b))),
            progress,
            compute_jobs: compute_jobs.max(1),
            verify_hash,
            rollsum,
        }
    }

    /// Sender role: announce our manifest, plan, and apply the plan.
    ///
    /// `source_root` is where the source files live on disk.
    ///
    /// # Errors
    ///
    /// Returns an error on transport, I/O, or protocol failure.
    pub(crate) async fn send<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
        &self,
        ctrl_send: &mut W,
        ctrl_recv: &mut R,
        source_manifest: &Manifest,
        source_root: &Path,
        options: &ExecutorOptions,
    ) -> Result<SyncStats> {
        let start = Instant::now();

        // Announce our source manifest (with the push target path).
        let file_list: Vec<FileMeta> = source_manifest
            .files
            .iter()
            .map(file_meta_from_entry)
            .collect();
        stream::send_frame(
            ctrl_send,
            &Frame::IndexRequest {
                file_list,
                path: options.remote_path.clone(),
                verify: options.remove_source_files || options.verify,
            },
        )
        .await?;

        // Receive the receiver's manifest.
        let frame = from_peer(stream::receive_frame(ctrl_recv).await?)?;
        let dest_manifest = match frame {
            Frame::IndexResponse { file_list } => manifest_from_file_meta(&file_list),
            other => {
                return Err(Error::Other(format!("Expected IndexResponse, got {other:?}")));
            }
        };

        // Plan.
        let planner = Planner::new(PlannerConfig {
            checksum: options.checksum,
            delete: options.delete,
            update_only: options.update_only,
            ignore_existing: options.ignore_existing,
            existing: options.existing,
            ignore_times: options.ignore_times,
            // `--no-times`: mtimes are not preserved, so the size+mtime quick
            // check would never agree — fall back to size-only (rsync).
            size_only: !options.preserve_times,
        });
        let plan = planner.plan(source_manifest, &dest_manifest);
        tracing::info!(
            "plan: {} create, {} update, {} delete, {} skip",
            plan.creates.len(),
            plan.updates.len(),
            plan.deletes.len(),
            plan.skips.len()
        );

        let (bytes, files, transferred, send_skipped) = self
            .apply_plan(ctrl_send, ctrl_recv, &plan, source_root, options)
            .await?;

        // Deletes. Excluded paths are protected from deletion (rsync
        // semantics); files are removed before the (now empty) directories.
        // Policy-skipped source paths (`source_manifest.skipped`) are also
        // protected, **including their whole destination subtree**: the path
        // still exists in the source (an external-dir link, say) — it is
        // simply not transferred under the current policy — so a `--delete`
        // run must not remove a previously created destination copy of it or
        // its contents (e.g. entries recursed by an earlier `--follow-links`
        // run).
        if !plan.deletes.is_empty() {
            let filter = FilterSet {
                includes: options.include.clone(),
                excludes: options.exclude.clone(),
            };
            // Policy-skipped source paths indexed for O(depth) protection
            // checks: a delete is dropped when the path itself or any of its
            // ancestor directories names a skipped path (a linear scan of
            // every skipped path per delete is O(deletes × skipped) — the
            // same shape as the old cross-file pairing scan).
            let skipped_set: std::collections::HashSet<&str> =
                source_manifest.skipped.iter().map(String::as_str).collect();
            let mut file_deletes: Vec<String> = Vec::new();
            let mut dir_deletes: Vec<String> = Vec::new();
            for task in &plan.deletes {
                let rel = wire_rel(&task.relative_path);
                let policy_skipped = is_under_skipped(&rel, &skipped_set);
                if !filter.passes(&rel) || policy_skipped {
                    continue;
                }
                if task.is_dir {
                    dir_deletes.push(rel);
                } else {
                    file_deletes.push(rel);
                }
            }
            // Remove children before parents.
            dir_deletes.sort_by(|a, b| b.cmp(a));
            let paths: Vec<String> = file_deletes.into_iter().chain(dir_deletes).collect();
            if !paths.is_empty() {
                stream::send_frame(ctrl_send, &Frame::DeleteRequest { paths }).await?;
            }
        }

        stream::send_frame(ctrl_send, &Frame::Done { files, bytes }).await?;

        // Wait for the receiver to acknowledge before the transfer can end.
        let (recv_skipped, recv_hashes) = match from_peer(stream::receive_frame(ctrl_recv).await?)? {
            Frame::Ack {
                skipped, hashes, ..
            } => (skipped, hashes),
            other => return Err(Error::Other(format!("Expected Ack, got {other:?}"))),
        };

        // Report files skipped on either side: ours (source unreadable,
        // vanished, delta error) plus the receiver's (locked destination,
        // path too long, ...).
        let mut skipped = send_skipped;
        skipped.extend(recv_skipped.clone());

        // Post-transfer verification (`--verify`) and source removal
        // (`--remove-source-files`): the receiver hashed every applied file
        // while writing it (BLAKE3, requested via `verify` in the
        // IndexRequest); we compare it against the hash we computed while
        // reading the source (the delta checksum, or the on-the-fly chunk
        // hasher — no re-read). A silent wire corruption is caught here, not
        // after the source is gone. Files the receiver skipped, and files
        // whose source changed mid-transfer (size mismatch, caught in
        // `apply_plan`), are never touched. Only `--remove-source-files`
        // deletes anything; `--verify` reports and stops.
        if options.remove_source_files || options.verify {
            let ack_hashes: HashMap<&str, [u8; 32]> =
                recv_hashes.iter().map(|(rel, h)| (rel.as_str(), *h)).collect();
            for (rel, source_hash) in &transferred {
                if recv_skipped.iter().any(|s| s.path == *rel) {
                    continue;
                }
                let Some(dest_hash) = ack_hashes.get(rel.as_str()).copied() else {
                    tracing::warn!("no verification hash from receiver for {rel}");
                    skipped.push(SkippedFile::new(rel.clone(), "no verification hash from receiver".to_string()));
                    continue;
                };
                if *source_hash != dest_hash {
                    tracing::warn!(
                        "hash mismatch for {rel}: destination does not match source"
                    );
                    skipped.push(SkippedFile::new(rel.clone(), "hash mismatch: destination does not match source".to_string()));
                    continue;
                }
                if !options.remove_source_files {
                    // Pure verification (`--verify`): nothing to delete.
                    tracing::info!("verified source {rel} (--verify)");
                    continue;
                }
                // Entries whose content lives outside the scan root
                // (`--follow-links` recursion, external dereferences) are never
                // deleted: the source path is not ours to remove.
                let entry = source_manifest.files.iter().find(|f| {
                    f.relative_path == *rel
                });
                if entry.is_some_and(|f| f.dereferenced) {
                    tracing::info!("external-origin source {rel}; not removed");
                    continue;
                }
                let full = source_root.join(rel);
                // The source must still match what was scanned: a same-size,
                // in-place modification after the scan would make the
                // transferred bytes (and the destination) stale.
                match tokio::fs::metadata(&full).await {
                    Ok(src_meta) => {
                        let expected = source_manifest.files.iter().find(|f| {
                            f.relative_path == *rel
                        });
                        let scan_mtime = expected.map(|f| crate::sync::wire::mtime_to_wire(f.mtime_sec));
                        let stable = match (expected, scan_mtime) {
                            (Some(entry), Some(scan_mtime)) => {
                                src_meta.len() == entry.size
                                    && crate::platform::fs::mtime_secs(&src_meta) == scan_mtime
                                    // Same source filesystem on both reads, so
                                    // the nanosecond remainder is exact.
                                    && crate::platform::fs::mtime_nsecs(&src_meta) == entry.mtime_nsec
                            }
                            // No manifest entry: nothing to compare against.
                            _ => true,
                        };
                        if !stable {
                            tracing::warn!("source changed since scan: {rel}; not removed");
                            skipped.push(SkippedFile::new(rel.clone(), "source changed since scan; not removed".to_string()));
                            continue;
                        }
                    }
                    // Already gone — nothing to delete, and nothing to verify.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        tracing::warn!("failed to stat source {rel}: {e}");
                        skipped.push(SkippedFile::new(rel.clone(), e.to_string()));
                        continue;
                    }
                }
                match tokio::fs::remove_file(&full).await {
                    Ok(()) => {
                        tracing::info!("verified and removed source {rel} (--remove-source-files)");
                    }
                    // Already gone (the source tree changed mid-sync).
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!("failed to remove source {rel}: {e}");
                        skipped.push(SkippedFile::new(rel.clone(), e.to_string()));
                    }
                }
            }
        }

        // Counts cannot exceed usize on any real sync.
        #[expect(clippy::cast_possible_truncation)]
        let files_sent = files as usize;
        Ok(SyncStats {
            files_sent,
            files_received: 0,
            bytes_transferred: bytes,
            duration: start.elapsed(),
            skipped,
        })
    }

    /// First pass over the plan: collect the files that will delta-transfer
    /// and request their basis signatures up front. Signatures are only
    /// generated for files that actually need them.
    async fn request_needed_signatures<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
        &self,
        ctrl_send: &mut W,
        ctrl_recv: &mut R,
        plan: &SyncPlan,
    ) -> Result<HashMap<String, Arc<Signature>>> {
        let mut need_sig: Vec<String> = Vec::new();
        for task in plan.tasks() {
            // Hard-link members are never delta-transferred (they become
            // `CreateLinks` entries), so their basis signatures are not needed.
            if task.link_to.is_some() {
                continue;
            }
            let dest_exists = task.action == SyncAction::Update;
            if determine_strategy(task.source_size, dest_exists) == TransferStrategy::Delta {
                need_sig.push(wire_rel(&task.relative_path));
            }
        }
        self.request_signatures(ctrl_send, ctrl_recv, &need_sig)
            .await
    }

    /// Create (or replace) empty directories up front; files create their own
    /// parents when staged, so ordering is only relevant for empty dirs.
    /// Returns the number of directories announced.
    async fn send_make_dirs<W: AsyncWrite + Unpin>(
        &self,
        ctrl_send: &mut W,
        plan: &SyncPlan,
    ) -> Result<u64> {
        let mut dir_paths: Vec<String> = Vec::new();
        for task in plan.tasks() {
            if task.is_dir {
                dir_paths.push(wire_rel(&task.relative_path));
            }
        }
        if dir_paths.is_empty() {
            return Ok(0);
        }
        let dir_count = dir_paths.len() as u64;
        stream::send_frame(ctrl_send, &Frame::MakeDir { paths: dir_paths }).await?;
        Ok(dir_count)
    }

    /// Send the collected symlink, hard link, and special definitions (no
    /// content): hard link targets must already exist on the receiver.
    async fn send_links<W: AsyncWrite + Unpin>(
        &self,
        ctrl_send: &mut W,
        links: Vec<LinkSpec>,
        hardlinks: Vec<crate::protocol::HardlinkSpec>,
        specials: Vec<crate::protocol::SpecialSpec>,
    ) -> Result<()> {
        if !links.is_empty() || !hardlinks.is_empty() || !specials.is_empty() {
            stream::send_frame(
                ctrl_send,
                &Frame::CreateLinks {
                    links,
                    hardlinks,
                    specials,
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Join the oldest in-flight small-file read and push its content into
    /// the batch buffer. An unreadable or vanished source file skips that
    /// file; the rest of the batch is unaffected.
    async fn drain_batch_read(
        &self,
        batch_reads: &mut VecDeque<BatchRead>,
        batch: &mut Vec<BatchItem>,
        batch_bytes: &mut u64,
        skipped: &mut Vec<SkippedFile>,
        batch_seq: &mut u64,
    ) -> Result<()> {
        let Some(read) = batch_reads.pop_front() else {
            return Ok(());
        };
        let rel = read.rel;
        match read.handle.await {
            Ok(Ok(data)) => {
                // The whole-file hash exists only when the post-transfer
                // comparison will consume it.
                let checksum = if self.verify_hash {
                    Some(*blake3::hash(&data).as_bytes())
                } else {
                    None
                };
                let len = data.len() as u64;
                batch.push(BatchItem {
                    // The batch's file id is a per-run sequence number: the
                    // receiver never matches batch records by id (the path
                    // is the key; chunked-stream routing uses the hashed
                    // ids, which batch records never enter).
                    file_id: *batch_seq,
                    file_path: rel,
                    data,
                    checksum,
                });
                *batch_bytes += len;
                *batch_seq += 1;
            }
            Ok(Err(e)) => {
                tracing::warn!("skipping {rel}: {e}");
                skipped.push(SkippedFile::new(rel, e.to_string()));
            }
            Err(e) => return Err(Error::Other(format!("Read task panicked: {e}"))),
        }
        Ok(())
    }

    /// Apply a plan on the sender side: emit recipes for each task.
    ///
    /// A failure on one file (unreadable or vanished source, delta error)
    /// skips that file and continues — the whole sync is not aborted by a
    /// single-file condition. Skipped files are returned for the final report,
    /// alongside the wire paths of every file whose content was actually
    /// transferred (the deletion set for `--remove-source-files`).
    async fn apply_plan<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
        &self,
        ctrl_send: &mut W,
        ctrl_recv: &mut R,
        plan: &SyncPlan,
        source_root: &Path,
        options: &ExecutorOptions,
    ) -> Result<(u64, u64, Vec<(String, [u8; 32])>, Vec<SkippedFile>)> {
        // The run's transferable file count (regular files — the display's
        // `[index/total]` denominator; dirs, links, and specials are not
        // "files").
        let files_total = plan
            .tasks()
            .filter(|t| {
                !t.is_dir && t.link_target.is_none() && t.link_to.is_none() && t.special.is_none()
            })
            .count() as u64;
        let mut bytes_transferred: u64 = 0;
        let mut files_sent: u64 = 0;
        let mut skipped: Vec<SkippedFile> = Vec::new();
        let mut transferred: Vec<(String, [u8; 32])> = Vec::new();

        let mut batch: Vec<BatchItem> = Vec::new();
        let mut batch_bytes: u64 = 0;
        // Per-run sequence for the batch records' file ids (see
        // `drain_batch_read`).
        let mut batch_seq: u64 = 0;
        // In-flight small-file reads feeding the batch (bounded window — see
        // `BATCH_READ_WINDOW`).
        let mut batch_reads: VecDeque<BatchRead> = VecDeque::new();
        // Symbolic links and `.lnk` shortcuts carry no content; they are
        // created after all files have been transferred (hard link targets
        // must exist).
        let mut links: Vec<LinkSpec> = Vec::new();
        let mut hardlinks: Vec<crate::protocol::HardlinkSpec> = Vec::new();
        // Specials (fifos/sockets/devices) are contentless too — batched and
        // created after all file content, only under `--archive` (Unix).
        let mut specials: Vec<crate::protocol::SpecialSpec> = Vec::new();

        // First pass: request basis signatures for the files that will
        // delta-transfer, up front (in bounded groups — see
        // `request_signatures`).
        let mut sig_map = self
            .request_needed_signatures(ctrl_send, ctrl_recv, plan)
            .await?;

        // Sliding window over in-flight delta computations: each one streams
        // the full source through chunk+hash on a blocking thread, so a
        // strictly serial loop would keep the CPU idle between wire sends on
        // fast links. Drained in plan order; bounded by the storage-aware
        // compute window (see `Sender::compute_jobs`).
        let mut pending: VecDeque<DeltaJob> = VecDeque::new();

        // Cross-file basis pairing ("1.iso" / "1.1.iso" siblings): a file
        // transferred earlier in this plan can be the delta basis for a
        // later one, so only one of them crosses the wire as full content.
        // Paired dependents are files with no destination basis (strategy
        // Copy) of at least CROSS_BASIS_MIN; the reference's signature is
        // its delta byproduct or a spawned job.
        //
        // The search is indexed instead of scanning every earlier plan
        // entry (a linear scan is O(plan²) — measured at 40 s on a flat
        // 100 K-file tree where every file shares the parent directory):
        // a reference shares the candidate's parent and extension, one
        // file stem a proper prefix of the other, and sizes within 2x.
        // Prefix stems ("1" for "1.1") are found by direct lookup of each
        // proper prefix; the inverted direction ("1.1" for "1" — a dotted
        // stem sorts before its prefix) is covered as every entry also
        // registers itself under its own proper prefixes. Both indices
        // hold only entries large enough to be a basis (>= half the
        // cross-file minimum — smaller ones can never satisfy the 2x
        // ratio), so the pass is O(plan × stem length).
        let (mut basis_senders, mut basis_receivers) = {
            let mut refs: HashMap<String, Vec<std::sync::mpsc::Sender<Signature>>> =
                HashMap::new();
            let mut deps: HashMap<String, (String, std::sync::mpsc::Receiver<Signature>)> =
                HashMap::new();
            let mut by_stem: HashMap<StemKey, Vec<StemEntry>> = HashMap::new();
            let mut extends: HashMap<StemKey, Vec<StemEntry>> = HashMap::new();
            for task in plan.tasks() {
                if task.is_dir
                    || task.link_target.is_some()
                    || task.link_to.is_some()
                    || task.special.is_some()
                {
                    continue;
                }
                let rel = wire_rel(&task.relative_path);
                let size = task.source_size;
                let dest_exists = task.action == SyncAction::Update;
                if determine_strategy(size, dest_exists) == TransferStrategy::Copy
                    && size >= CROSS_BASIS_MIN
                    && let Some((ref_rel, ref_size)) =
                        indexed_candidate(&by_stem, &extends, &rel, size)
                {
                    let (tx, rx) = std::sync::mpsc::channel();
                    refs.entry(ref_rel.clone()).or_default().push(tx);
                    tracing::debug!(
                        "cross-file basis: {ref_rel} ({ref_size} B) -> {rel} ({size} B)"
                    );
                    deps.insert(rel.clone(), (ref_rel, rx));
                }
                register_basis_entry(&mut by_stem, &mut extends, &rel, size);
            }
            (refs, deps)
        };

        // Empty directories up front; files create their own parents when
        // staged, so ordering is only relevant for empty dirs.
        files_sent += self.send_make_dirs(ctrl_send, plan).await?;

        for task in plan.tasks() {
            if task.is_dir {
                continue; // handled by MakeDir above
            }
            // Symlinks / .lnk shortcuts: batched into the CreateLinks frame
            // (no content). The kind and the rewritten target were decided at
            // scan time (spec §3.2).
            if let Some(target) = &task.link_target {
                links.push(LinkSpec {
                    path: wire_rel(&task.relative_path),
                    target: target.clone(),
                    kind: task.link_kind,
                });
                continue;
            }
            // Hard links: batched too; the target is the root-relative path
            // of the representative file, transferred (or already present).
            if let Some(target) = &task.link_to {
                hardlinks.push(crate::protocol::HardlinkSpec {
                    path: wire_rel(&task.relative_path),
                    target: target.clone(),
                });
                continue;
            }
            // Specials: contentless and only sent under `--archive`. They
            // must *never* reach the content path — opening a fifo for read
            // would block forever — so without `-a` they are simply ignored.
            if let Some((kind, rdev)) = task.special {
                if options.archive {
                    specials.push(crate::protocol::SpecialSpec {
                        path: wire_rel(&task.relative_path),
                        kind,
                        rdev,
                    });
                }
                continue;
            }
            // The on-disk source: entries pulled in through `--follow-links`
            // recursion live outside the scan root and carry an explicit
            // source path; everything else resolves under `source_root`.
            let full = task_source(source_root, task);
            let dest_exists = task.action == SyncAction::Update;
            let strategy = determine_strategy(task.source_size, dest_exists);
            let rel_display = wire_rel(&task.relative_path);

            // Cross-file basis role: this task's dependents' channels (it is
            // a reference), and/or its own basis receiver + reference path
            // (it is a dependent).
            let ref_txs = basis_senders.remove(&rel_display).unwrap_or_default();
            let cross_basis = basis_receivers.remove(&rel_display);

            // Batchable: a fresh (Copy) file at or below the small-file
            // bound. Cross-file basis candidates (a reference with
            // dependents, or a dependent itself) stay on the delta path —
            // the batch carries no signature byproduct, and losing their
            // channels would degrade every chained dependent.
            let is_batchable = strategy == TransferStrategy::Copy
                && classify_file_size(task.source_size) == FileClass::Small
                && ref_txs.is_empty()
                && cross_basis.is_none();

            if is_batchable {
                // Bounded read-ahead: drain the oldest read when the window is
                // full, so the reads overlap each other and the wire.
                if batch_reads.len() >= BATCH_READ_WINDOW {
                    self.drain_batch_read(
                        &mut batch_reads,
                        &mut batch,
                        &mut batch_bytes,
                        &mut skipped,
                        &mut batch_seq,
                    )
                    .await?;
                }
                // The branch always continues, so the top-of-loop `full` and
                // `rel_display` are moved here rather than recomputed.
                batch_reads.push_back(BatchRead {
                    rel: rel_display,
                    handle: tokio::spawn(async move {
                        tokio::fs::read(&full).await.map_err(Error::Io)
                    }),
                });
                // Bound the batch frame size (see `BATCH_BYTES_BUDGET`):
                // flush eagerly once the budget is crossed.
                if batch_bytes >= BATCH_BYTES_BUDGET {
                    self.flush_batch(
                        ctrl_send,
                        &mut batch,
                        &mut batch_reads,
                        &mut batch_bytes,
                        &mut bytes_transferred,
                        &mut files_sent,
                        &mut transferred,
                        &mut skipped,
                        options,
                        &mut batch_seq,
                        files_total,
                    )
                    .await?;
                }
                continue;
            }

            // Non-batchable (delta or chunked): the accumulated batch is
            // *not* flushed here. Batch files are ≤ 2 MiB (below the
            // cross-file minimum), so none can be
            // a cross-file delta basis — reordering them past the delta and
            // chunked work is wire-safe, and flushing per interruption would
            // fragment the batch into thousands of tiny frames on a mixed
            // plan (measured: a 10 GiB small/medium/large tree dropped from
            // 43 s to ~27 s just by class-sorting the scan order). The batch
            // drains at the 128 MiB budget or at the end of the plan.

            // The batch path needs no file id (batch records carry a
            // per-run sequence number), so the hash is computed only here —
            // over the already-normalized wire path rather than re-converting
            // the task's `PathBuf`.
            let fid = file_id(&rel_display);
            let outcome = if let Some((basis_rel, basis_rx)) = cross_basis {
                // Cross-file delta: a sibling transferred earlier in this
                // plan is the basis. Its signature arrives via the channel
                // (delta byproduct or a spawned job); the receiver applies
                // against the sibling file.
                if pending.len() >= self.compute_jobs {
                    self.drain_delta(
                        ctrl_send,
                        &mut pending,
                        &mut bytes_transferred,
                        &mut files_sent,
                        &mut transferred,
                        &mut skipped,
                        files_total,
                    )
                    .await?;
                }
                let display = rel_display.clone();
                let progress = self.progress.clone();
                let full_compute = full.clone();
                let verify_hash = self.verify_hash;
                // This file is also a reference (a later plan entry uses it
                // as a cross-file basis — chains like f1 -> f100 -> f1000).
                // Its cross-file delta produces no byproduct, so its own
                // dependents are fed by a spawned signature job — dropping
                // `ref_txs` here would lose every chained dependent.
                if !ref_txs.is_empty() {
                    spawn_ref_signature_job(&full_compute, ref_txs);
                }
                pending.push_back(DeltaJob {
                    rel: rel_display,
                    full: full.clone(),
                    fid,
                    expected_size: task.source_size,
                    basis_txs: Vec::new(),
                    handle: tokio::task::spawn_blocking(move || {
                        compute_cross_delta_job(
                            &full_compute,
                            basis_rx,
                            basis_rel,
                            display,
                            progress.as_ref(),
                            verify_hash,
                            files_total,
                        )
                    }),
                });
                Ok(SendOutcome::Deferred)
            } else if strategy == TransferStrategy::Delta
                || (strategy == TransferStrategy::Copy
                    && classify_file_size(task.source_size) != FileClass::Large)
            {
                // Delta transfers and medium whole-file copies share the
                // computation window: each streams the file on a blocking
                // thread, so the reads overlap the wire sends of the
                // previously drained jobs (a strictly serial loop would leave
                // the wire starved between files).
                if pending.len() >= self.compute_jobs {
                    self.drain_delta(
                        ctrl_send,
                        &mut pending,
                        &mut bytes_transferred,
                        &mut files_sent,
                        &mut transferred,
                        &mut skipped,
                        files_total,
                    )
                    .await?;
                }
                let is_delta = strategy == TransferStrategy::Delta;
                // The signature is *removed* as its job takes it: the map
                // holds every group's signatures, and each is only needed
                // by its one computation, so the sender's peak is the
                // window plus one request group — not all of them.
                let sig = sig_map.remove(&rel_display);
                let display = rel_display.clone();
                let progress = self.progress.clone();
                // Two owned copies: the job keeps one for the chunked
                // fallback at drain time, the computation task opens the
                // other (the loop's `full` stays for the error arms).
                let full_job = full.clone();
                let full_compute = full.clone();
                let verify_hash = self.verify_hash;
                let rollsum = self.rollsum;
                // A delta-path reference's source signature (the free
                // byproduct) feeds its cross-file dependents; a medium
                // whole-literal reference has no byproduct, so a signature
                // job runs in parallel with the transfer instead.
                let ref_txs_for_job = if is_delta {
                    ref_txs
                } else {
                    if !ref_txs.is_empty() {
                        spawn_ref_signature_job(&full_compute, ref_txs);
                    }
                    Vec::new()
                };
                pending.push_back(DeltaJob {
                    rel: rel_display,
                    full: full_job,
                    fid,
                    expected_size: task.source_size,
                    basis_txs: ref_txs_for_job,
                    handle: tokio::task::spawn_blocking(move || {
                        if is_delta {
                            compute_delta_job(
                                &full_compute,
                                sig,
                                display,
                                progress.as_ref(),
                                verify_hash,
                                rollsum,
                                files_total,
                            )
                        } else {
                            // Medium new file: the whole content as one
                            // literal delta (bounded by the medium tier).
                            read_whole_delta(
                                &full_compute,
                                &display,
                                progress.as_ref(),
                                verify_hash,
                                files_total,
                            )
                        }
                    }),
                });
                Ok(SendOutcome::Deferred)
            } else {
                // Large new file: stream as sequential chunks — bounded
                // memory, and an interrupt leaves a resumable partial. A
                // cross-file reference gets its signature job alongside.
                if !ref_txs.is_empty() {
                    spawn_ref_signature_job(&full, ref_txs);
                }
                self.send_chunked(ctrl_send, fid, &rel_display, &full, task.source_size, files_total)
                    .await
                    .map(SendOutcome::Sent)
            };
            match outcome {
                Ok(SendOutcome::Sent((sent, hash))) => {
                    bytes_transferred += sent;
                    files_sent += 1;
                    // A size mismatch means the source changed between the
                    // scan and the transfer — the destination holds a stale
                    // snapshot, so the source must never be deleted.
                    if sent != task.source_size {
                        source_changed(&wire_rel(&task.relative_path), &mut skipped);
                        continue;
                    }
                    transferred.push((wire_rel(&task.relative_path), hash));
                }
                // Deferred deltas are drained in plan order; their per-file
                // outcomes (including skips) are folded by `drain_delta`.
                Ok(SendOutcome::Deferred) => {}
                // File-level failures (source unreadable/vanished, delta
                // computation) skip the file; transport/protocol errors are
                // connection-level and still abort.
                Err(Error::Io(e)) => {
                    skip_file(&full, &wire_rel(&task.relative_path), &e, &mut skipped);
                }
                Err(Error::Delta(e)) => {
                    skip_file(&full, &wire_rel(&task.relative_path), &e, &mut skipped);
                }
                Err(e) => return Err(e),
            }
        }

        // Flush the delta window: join and send every remaining computation
        // (in plan order).
        while !pending.is_empty() {
            self.drain_delta(
                ctrl_send,
                &mut pending,
                &mut bytes_transferred,
                &mut files_sent,
                &mut transferred,
                &mut skipped,
                files_total,
            )
            .await?;
        }

        // Flush any remaining batch.
        self.flush_batch(
            ctrl_send,
            &mut batch,
            &mut batch_reads,
            &mut batch_bytes,
            &mut bytes_transferred,
            &mut files_sent,
            &mut transferred,
            &mut skipped,
            options,
            &mut batch_seq,
            files_total,
        )
        .await?;


        // Links last: hard link targets must already exist on the receiver.
        self.send_links(ctrl_send, links, hardlinks, specials).await?;

        Ok((bytes_transferred, files_sent, transferred, skipped))
    }

    /// Ask the receiver for basis signatures of `paths`, in bounded groups.
    ///
    /// Signatures are wrapped in `Arc` so each delta task can share the chunk
    /// table with the `spawn_blocking` worker without copying it per file.
    async fn request_signatures<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
        &self,
        ctrl_send: &mut W,
        ctrl_recv: &mut R,
        paths: &[String],
    ) -> Result<HashMap<String, Arc<Signature>>> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        let mut out = HashMap::with_capacity(paths.len());
        for group in paths.chunks(SIGNATURE_GROUP) {
            stream::send_frame(
                ctrl_send,
                &Frame::SignatureRequest {
                    paths: group.to_vec(),
                },
            )
            .await?;
            let frame = from_peer(stream::receive_frame(ctrl_recv).await?)?;
            match frame {
                Frame::SignatureResponse { signatures } => {
                    out.extend(signatures.into_iter().map(|s: SignatureEntry| {
                        (s.file_path, Arc::new(s.signature))
                    }));
                }
                other => Err(Error::Other(format!(
                    "Expected SignatureResponse, got {other:?}"
                )))?,
            }
        }
        Ok(out)
    }

    /// Flush the small-file batch buffer as one compressed frame. Any
    /// in-flight reads are joined first, so the frame carries every
    /// accumulated file. The extra counters are the price of reporting
    /// per-file results; the established `#[expect]` pattern (see
    /// `FileMeta::new`) applies.
    #[expect(clippy::too_many_arguments)]
    async fn flush_batch<W: AsyncWrite + Unpin>(
        &self,
        ctrl_send: &mut W,
        batch: &mut Vec<BatchItem>,
        batch_reads: &mut VecDeque<BatchRead>,
        batch_bytes: &mut u64,
        bytes_transferred: &mut u64,
        files_sent: &mut u64,
        transferred: &mut Vec<(String, [u8; 32])>,
        skipped: &mut Vec<SkippedFile>,
        options: &ExecutorOptions,
        batch_seq: &mut u64,
        files_total: u64,
    ) -> Result<()> {
        while !batch_reads.is_empty() {
            self.drain_batch_read(batch_reads, batch, batch_bytes, skipped, batch_seq)
                .await?;
        }
        if batch.is_empty() {
            return Ok(());
        }
        // Report each batched file as complete (small files: instant).
        if let Some(report) = &self.progress {
            for item in batch.iter() {
                let size = item.data.len() as u64;
                report(&item.file_path, size, size, files_total);
            }
        }
        // Every file in the batch was transferred (unreadable sources were
        // skipped before accumulating) — record rel + source hash for
        // `--remove-source-files` verification (only computed when the
        // comparison will consume it, so the default mode carries zeros).
        transferred.extend(
            batch
                .iter()
                .map(|b| (b.file_path.clone(), b.checksum.unwrap_or([0u8; 32]))),
        );
        let files = batch.len() as u64;
        let bytes = std::mem::take(batch_bytes);
        if let Some(limiter) = &self.limiter {
            limiter.acquire(bytes).await;
        }
        let items = std::mem::take(batch);
        if options.compress {
            // The compressed path needs the whole payload anyway — serialize
            // the batch as the postcard frame (the existing route). The
            // buffers are consumed, not copied: `push_literal_owned` moves
            // each file's data into its delta.
            let recipes = items
                .into_iter()
                .map(|b| {
                    let mut delta = Delta::new(b.data.len() as u64, 0);
                    delta.checksum = b.checksum;
                    delta.push_literal_owned(b.data);
                    BatchFile {
                        file_id: b.file_id,
                        file_path: b.file_path,
                        delta,
                    }
                })
                .collect();
            stream::send_frame_compressed(
                ctrl_send,
                &Frame::Batch { recipes },
                true,
                64 * 1024,
            )
            .await?;
        } else {
            // Default path: the zero-copy raw layout — the file data goes
            // straight from its buffer to the wire (no postcard pass, no
            // full-payload copy).
            stream::send_batch_raw(ctrl_send, &items).await?;
        }
        *bytes_transferred += bytes;
        *files_sent += files;
        Ok(())
    }

    /// Stream a large new file as sequential chunk frames (bounded memory;
    /// an interrupted transfer leaves a resumable partial on the receiver).
    ///
    /// The disk read of chunk N+1 is launched before chunk N is sent, so the
    /// wire is never starved by the source disk: read and send overlap
    /// instead of alternating (on a fast link the alternation would cap
    /// throughput at the read latency).
    async fn send_chunked<W: AsyncWrite + Unpin>(
        &self,
        ctrl: &mut W,
        fid: u64,
        display: &str,
        full: &Path,
        size: u64,
        files_total: u64,
    ) -> Result<(u64, [u8; 32])> {
        stream::send_frame(
            ctrl,
            &Frame::FileStart {
                file_id: fid,
                file_path: display.to_string(),
                size,
            },
        )
        .await?;

        let mut file = std::fs::File::open(full).map_err(Error::Io)?;
        let mut bufs = [vec![0u8; CHUNK_SIZE], vec![0u8; CHUNK_SIZE]];
        let mut sent = 0u64;
        // Bulk the raw chunk frames and write them in ~8 MiB batches. A
        // frame-at-a-time `write_all` keeps only the pipe + socket buffer
        // in flight, so a real-network transfer caps at in-flight / RTT
        // (measured ~100 MB/s vs rsync's 650 MB/s over the same link);
        // batching raises the in-flight data to the batch size. The
        // receiver is frame-delimited, so the batching is wire-transparent.
        let mut wire_buf = Vec::with_capacity(CHUNK_BATCH_BYTES + CHUNK_SIZE);
        // The verification hash of the whole file, computed only when
        // `--verify`/`--remove-source-files` will consume it (the returned
        // hash feeds the post-transfer comparison; without those flags it is
        // dead work). When on, the hasher rides the read-ahead tasks: they
        // run strictly one at a time in chunk order, so the updates are
        // byte-ordered — the value is the canonical sequential BLAKE3 —
        // while the hashing overlaps the wire sends (the transfer already
        // touches every byte, so verification costs no re-read).
        let mut hasher = self.verify_hash.then(blake3::Hasher::new);
        // Prime the pipeline: the first chunk is read and hashed before the
        // loop (a single blocking read on the reactor — the loop has nothing
        // else to do yet, and every subsequent chunk is covered by the
        // read-ahead).
        let mut len = file.read(&mut bufs[0]).map_err(Error::Io)?;
        if let Some(h) = &mut hasher {
            h.update(&bufs[0][..len]);
        }
        let mut cur = 0usize;
        while len > 0 {
            let next = 1 - cur;
            // Read-ahead: the disk read (and the hash, when verification is
            // on) of chunk N+1 run on a blocking thread while chunk N is on
            // the wire. The (file, hasher, buffer) triple moves into the task
            // and back; the other buffer stays in the loop for the send.
            let mut file_slot = Some(file);
            let mut hasher_slot = hasher;
            let mut ahead = std::mem::take(&mut bufs[next]);
            let read_task = tokio::task::spawn_blocking(move || {
                let mut f = file_slot.take().expect("send_chunked file");
                let mut h = hasher_slot.take();
                let r = f.read(&mut ahead).map_err(Error::Io);
                if let (Ok(read), Some(h)) = (&r, &mut h) {
                    h.update(&ahead[..*read]);
                }
                (r, h, f, ahead)
            });
            if let Some(limiter) = &self.limiter {
                limiter.acquire(len as u64).await;
            }
            // Zero-copy chunk frame: the buffer goes straight to the wire
            // (bit 30 layout — no postcard serialization pass), batched as
            // raw bytes so the write never waits per 1 MiB frame.
            stream::chunk_frame_wire(fid, &bufs[cur][..len], &mut wire_buf)?;
            if wire_buf.len() >= CHUNK_BATCH_BYTES {
                ctrl.write_all(&wire_buf).await?;
                wire_buf.clear();
            }
            sent += len as u64;
            if let Some(report) = &self.progress {
                report(display, sent, size, files_total);
            }
            // Join the read-ahead before the next iteration.
            let (read_result, hasher_back, file_back, buf_back) = read_task
                .await
                .map_err(|e| Error::Other(format!("Read task panicked: {e}")))?;
            file = file_back;
            hasher = hasher_back;
            bufs[next] = buf_back;
            len = read_result?;
            cur = next;
        }

        // The batch remainder, then the end marker.
        if !wire_buf.is_empty() {
            ctrl.write_all(&wire_buf).await?;
        }
        stream::send_frame(ctrl, &Frame::FileEnd { file_id: fid }).await?;
        let checksum = hasher.map_or([0u8; 32], |h| *h.finalize().as_bytes());
        Ok((sent, checksum))
    }

    /// Send the oldest in-flight delta computation: join it, transmit the
    /// recipe (or fall back to the chunked stream when the basis was
    /// useless), and fold the per-file outcome into the running counters.
    #[expect(clippy::too_many_arguments, reason = "every running counter folds into this one point")]
    async fn drain_delta<W: AsyncWrite + Unpin>(
        &self,
        ctrl_send: &mut W,
        pending: &mut VecDeque<DeltaJob>,
        bytes_transferred: &mut u64,
        files_sent: &mut u64,
        transferred: &mut Vec<(String, [u8; 32])>,
        skipped: &mut Vec<SkippedFile>,
        files_total: u64,
    ) -> Result<()> {
        let Some(job) = pending.pop_front() else {
            return Ok(());
        };
        let rel = job.rel;
        let sent: Result<(u64, [u8; 32], Option<Signature>)> = match job.handle.await {
            Ok(Ok(PreparedDelta::Delta(delta, source_signature, basis_path))) => {
                let source_size = delta.source_size;
                let checksum = delta.checksum.unwrap_or([0u8; 32]);
                let literal_bytes = delta.bytes_literal();
                // A cross-file pair that turns out dissimilar (matched less
                // than half) would ship nearly the whole file as literals —
                // stream it instead.
                let dissimilar = basis_path.is_some()
                    && delta.bytes_matched() * 2 < delta.source_size;
                if literal_bytes > DELTA_LITERAL_CHUNK_LIMIT || dissimilar {
                    // Belt and suspenders — the budget aborts earlier; a
                    // crossing would be one giant in-memory frame.
                    self.send_chunked(
                        ctrl_send,
                        job.fid,
                        &rel,
                        &job.full,
                        job.expected_size,
                        files_total,
                    )
                    .await
                    .map(|(sent, hash)| (sent, hash, None))
                } else {
                    // Only literal bytes cross the wire; pace by those. The
                    // receiver reconstructs the full file, so count source
                    // bytes.
                    if let Some(limiter) = &self.limiter {
                        limiter.acquire(literal_bytes).await;
                    }
                    stream::send_frame(
                        ctrl_send,
                        &Frame::DeltaRecipe {
                            file_id: job.fid,
                            file_path: rel.clone(),
                            delta,
                            source_signature: source_signature.clone(),
                            basis_path,
                        },
                    )
                    .await?;
                    Ok((source_size, checksum, source_signature))
                }
            }
            Ok(Ok(PreparedDelta::ChunkedFallback)) => {
                // No useful basis (or the literal budget was crossed): stream
                // the whole file instead of one giant frame.
                let size = tokio::fs::metadata(&job.full)
                    .await
                    .map_err(Error::Io)?
                    .len();
                self.send_chunked(ctrl_send, job.fid, &rel, &job.full, size, files_total)
                    .await
                    .map(|(sent, hash)| (sent, hash, None))
            }
            Ok(Err(e)) => Err(e),
            Err(e) => return Err(Error::Other(format!("Delta task panicked: {e}"))),
        };
        match sent {
            Ok((sent, hash, source_signature)) => {
                *bytes_transferred += sent;
                *files_sent += 1;
                // A size mismatch means the source changed between the scan
                // and the transfer — the destination holds a stale snapshot,
                // so the source must never be deleted.
                if sent == job.expected_size {
                    // Cross-file dependents now have a valid basis on the
                    // receiver (this file was applied): hand them its source
                    // signature (the free byproduct) so their delta jobs can
                    // proceed. On any skip/failure the senders drop and the
                    // dependents' channel recv fails — they skip too.
                    if let Some(sig) = source_signature {
                        for tx in &job.basis_txs {
                            let _ = tx.send(sig.clone());
                        }
                    }
                    transferred.push((rel, hash));
                } else {
                    source_changed(&rel, skipped);
                }
            }
            // File-level failures (source unreadable/vanished, delta
            // computation) skip the file; transport/protocol errors are
            // connection-level and still abort.
            Err(Error::Io(e)) => skip_file(Path::new(&rel), &rel, &e, skipped),
            Err(Error::Delta(e)) => skip_file(Path::new(&rel), &rel, &e, skipped),
            Err(e) => return Err(e),
        }
        Ok(())
    }

}

/// Fold a file-level send failure into the skip list (warn with the display
/// path, record the wire-relative path so the receiver can match it).
/// Transport/protocol errors are connection-level and still abort.
fn skip_file(shown: &Path, rel: &str, e: &dyn std::fmt::Display, skipped: &mut Vec<SkippedFile>) {
    tracing::warn!("skipping {}: {e}", shown.display());
    skipped.push(SkippedFile::new(rel.to_string(), e.to_string()));
}

/// A size mismatch means the source changed between the scan and the
/// transfer — the destination holds a stale snapshot, so the source must
/// never be deleted.
fn source_changed(rel: &str, skipped: &mut Vec<SkippedFile>) {
    skipped.push(SkippedFile::new(
        rel.to_string(),
        "source changed during transfer; not removed".to_string(),
    ));
}

/// Whether `rel` is a policy-skipped source path or lies under one (the
/// `--delete` protection check): true when the path itself or any ancestor
/// directory names a skipped path. Equivalent to the old per-delete linear
/// scan (`rel == s || rel.starts_with(s + "/")`), but O(depth) hash lookups.
fn is_under_skipped(rel: &str, skipped: &std::collections::HashSet<&str>) -> bool {
    let mut cur = rel;
    loop {
        if skipped.contains(cur) {
            return true;
        }
        match cur.rfind('/') {
            Some(i) => cur = &cur[..i],
            None => return false,
        }
    }
}

/// Result of one task's send: the content went out immediately, or the delta
/// was deferred into the computation window (drained in plan order).
enum SendOutcome {
    /// Bytes sent (`(sent_bytes, source_checksum)`).
    Sent((u64, [u8; 32])),
    /// The delta computation is in flight in the window.
    Deferred,
}

/// One in-flight small-file read for the batch frame (windowed — see
/// `BATCH_READ_WINDOW`).
struct BatchRead {
    /// Wire-relative path (frame path + skip report identity).
    rel: String,
    /// The running read (owns the on-disk source path).
    handle: tokio::task::JoinHandle<Result<Vec<u8>>>,
}

/// One in-flight delta computation (windowed: the source disk and CPU are
/// shared, and each job holds up to the literal budget of memory).
struct DeltaJob {
    /// Wire-relative path (frame path + report identity).
    rel: String,
    /// On-disk source (resolved at spawn time).
    full: PathBuf,
    /// Stable identifier from the scan.
    fid: u64,
    /// The scanned size, compared against the sent size after the join (a
    /// mismatch means the source changed mid-transfer).
    expected_size: u64,
    /// Cross-file basis: channels the job's source signature (the free
    /// byproduct) is forwarded into, for sibling files that delta against
    /// this one. Dropping them on failure makes the dependents fail too.
    basis_txs: Vec<std::sync::mpsc::Sender<Signature>>,
    /// The running computation.
    handle: tokio::task::JoinHandle<Result<PreparedDelta>>,
}

/// What a delta computation produced.
enum PreparedDelta {
    /// The delta is ready to transmit, plus the sender's chunk signature of
    /// the source — the basis signature the new destination content will
    /// have (the receiver caches it; `None` when the source wasn't chunked)
    /// — and, for cross-file deltas, the sibling file the Copy ops
    /// reference (the receiver applies against that file instead of the
    /// file itself).
    Delta(Delta, Option<Signature>, Option<String>),
    /// The basis was useless (missing/empty, or the literal budget was
    /// crossed): the caller falls back to the chunked stream.
    ChunkedFallback,
}

/// Read a medium file whole into a single-literal delta on a blocking thread
/// (bounded by the medium tier, ≤ 16 MiB). With `verify_hash`, the delta's
/// checksum is the BLAKE3 of the whole file, computed while reading it.
fn read_whole_delta(
    full: &Path,
    display: &str,
    progress: Option<&ProgressFn>,
    verify_hash: bool,
    files_total: u64,
) -> Result<PreparedDelta> {
    let mut file = std::fs::File::open(full).map_err(Error::Io)?;
    let len = file.metadata().map_err(Error::Io)?.len();
    let mut delta = Delta::new(len, 0);
    let mut hasher = verify_hash.then(blake3::Hasher::new);
    let mut buf = vec![0u8; 256 * 1024];
    let mut read = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        if let Some(h) = &mut hasher {
            h.update(&buf[..n]);
        }
        delta.push_literal(&buf[..n]);
        read += n as u64;
        if let Some(report) = progress {
            report(display, read, len, files_total);
        }
    }
    delta.checksum = hasher.map(|h| *h.finalize().as_bytes());
    Ok(PreparedDelta::Delta(delta, None, None))
}

/// Compute a delta for one file on a blocking thread: stream the source
/// through the budgeted engine. A missing or empty basis means the whole file
/// would be one in-memory literal — [`PreparedDelta::ChunkedFallback`] lets
/// the caller stream it instead. `verify_hash` requests the whole-file
/// checksum (consumed by the post-transfer comparison).
fn compute_delta_job(
    full: &Path,
    sig: Option<Arc<Signature>>,
    display: String,
    progress: Option<&ProgressFn>,
    verify_hash: bool,
    rollsum: bool,
    files_total: u64,
) -> Result<PreparedDelta> {
    let Some(sig) = sig else {
        return Ok(PreparedDelta::ChunkedFallback);
    };
    if sig.file_size == 0 || sig.chunks.is_empty() {
        // Empty basis (missing, directory, or empty destination): checked up
        // front so a large source never becomes one in-memory literal.
        return Ok(PreparedDelta::ChunkedFallback);
    }
    let file = std::fs::File::open(full).map_err(Error::Io)?;
    let len = file.metadata().map_err(Error::Io)?.len();
    let mut source: Box<dyn std::io::Read> = match progress {
        Some(report) => {
            Box::new(crate::sync::executor::ProgressStream::new(
                file,
                display,
                len,
                files_total,
                Arc::clone(report),
            ))
        }
        None => Box::new(file),
    };
    // The rollsum engine (rsync-style): fixed blocks + byte-sliding scan.
    // The signature format differs (weak checksums present), the delta op
    // format is identical, and there is no chunk-signature byproduct (the
    // sigCache stays a CDC feature on this branch).
    if rollsum {
        return match compute_delta_rollsum(&mut source, &sig, DELTA_LITERAL_CHUNK_LIMIT, verify_hash)
            .map_err(Error::from)
        {
            Ok(delta) => Ok(PreparedDelta::Delta(delta, None, None)),
            Err(Error::Delta(DeltaError::LiteralBudgetExceeded { .. })) => {
                Ok(PreparedDelta::ChunkedFallback)
            }
            Err(e) => Err(e),
        };
    }
    // Budgeted: a basis that matches nothing aborts at the limit instead of
    // accumulating the whole file as one literal. The source's chunk
    // signature is collected as a free byproduct (the per-chunk hashes are
    // already computed for matching) — the receiver caches it so the next
    // run's basis signing can skip re-reading the destination.
    let mut source_sig = Vec::new();
    match compute_delta_limited(
        &mut source,
        &sig,
        DELTA_LITERAL_CHUNK_LIMIT,
        verify_hash,
        Some(&mut source_sig),
    )
    .map_err(Error::from)
    {
        Ok(delta) => {
            let source_signature = (!source_sig.is_empty()).then_some(Signature {
                file_size: delta.source_size,
                chunks: source_sig,
            });
            Ok(PreparedDelta::Delta(delta, source_signature, None))
        }
        Err(Error::Delta(DeltaError::LiteralBudgetExceeded { .. })) => {
            Ok(PreparedDelta::ChunkedFallback)
        }
        Err(e) => Err(e),
    }
}

/// Minimum size for cross-file basis pairing — below this the delta
/// machinery costs more than the whole-file transfer it would save.
const CROSS_BASIS_MIN: u64 = 1024 * 1024;

/// Index key for the cross-file basis pairing: (parent directory,
/// extension, file stem) of a wire-relative path. Parent and extension
/// must match exactly; the stem is lossy, which is faithful to the old
/// scan (it compared lossy stems too).
type StemKey = (String, Option<String>, String);

/// One registered entry in the cross-file basis index.
struct StemEntry {
    size: u64,
    rel: String,
}

/// A wire-relative path's index key.
fn stem_key(rel: &str) -> StemKey {
    let path = Path::new(rel);
    (
        path.parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path.extension().map(|e| e.to_string_lossy().into_owned()),
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
}

/// Register `rel` (of size `size`) in the basis index so later tasks can
/// use it as a cross-file reference. Entries below half the cross-file
/// minimum can never satisfy the 2x size ratio for any candidate (every
/// candidate is at least `CROSS_BASIS_MIN`) and are skipped.
fn register_basis_entry(
    by_stem: &mut HashMap<StemKey, Vec<StemEntry>>,
    extends: &mut HashMap<StemKey, Vec<StemEntry>>,
    rel: &str,
    size: u64,
) {
    if size < CROSS_BASIS_MIN / 2 {
        return;
    }
    let (parent, ext, stem) = stem_key(rel);
    by_stem
        .entry((parent.clone(), ext.clone(), stem.clone()))
        .or_default()
        .push(StemEntry { size, rel: rel.to_string() });
    // The inverted direction: this entry can be the reference for a later
    // task whose stem is one of its proper prefixes ("1.1.iso" for
    // "1.iso" — the dotted name sorts before its prefix, so only the
    // inversion finds it). Register under every proper prefix.
    for i in 1..stem.len() {
        if !stem.is_char_boundary(i) {
            continue;
        }
        extends
            .entry((parent.clone(), ext.clone(), stem[..i].to_string()))
            .or_default()
            .push(StemEntry { size, rel: rel.to_string() });
    }
}

/// Find the best cross-file basis for `(rel, size)` among earlier plan
/// entries: same parent directory, same extension, one file stem a proper
/// prefix of the other ("1.iso" / "1.1.iso"), and sizes within 2x. The
/// closest in size wins (ties keep the plan-earlier entry).
fn indexed_candidate(
    by_stem: &HashMap<StemKey, Vec<StemEntry>>,
    extends: &HashMap<StemKey, Vec<StemEntry>>,
    rel: &str,
    size: u64,
) -> Option<(String, u64)> {
    let key = stem_key(rel);
    let mut best: Option<(u64, String, u64)> = None; // (abs diff, rel, ref size)
    let mut consider = |ref_rel: &str, ref_size: u64| {
        let (lo, hi) = (size.min(ref_size), size.max(ref_size));
        let diff = ref_size.abs_diff(size);
        if hi <= lo.saturating_mul(2) && best.as_ref().is_none_or(|(d, ..)| diff < *d) {
            best = Some((diff, ref_rel.to_string(), ref_size));
        }
    };
    // Prefix stems ("1" for "1.1").
    for i in 1..key.2.len() {
        if !key.2.is_char_boundary(i) {
            continue;
        }
        if let Some(entries) = by_stem.get(&(key.0.clone(), key.1.clone(), key.2[..i].to_string()))
        {
            for e in entries {
                consider(&e.rel, e.size);
            }
        }
    }
    // Extending stems ("1.1" for "1"), registered by earlier entries.
    if let Some(entries) = extends.get(&key) {
        for e in entries {
            consider(&e.rel, e.size);
        }
    }
    best.map(|(_, ref_rel, ref_size)| (ref_rel, ref_size))
}

/// Compute a reference file's chunk signature on a blocking thread —
/// parallel with its own transfer — and hand it to the cross-file
/// dependents. On failure the senders drop and the dependents' channel
/// recv fails, so they skip with a warning instead of corrupting.
fn spawn_ref_signature_job(full: &Path, txs: Vec<std::sync::mpsc::Sender<Signature>>) {
    let full = full.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let Ok(mut file) = std::fs::File::open(&full) else {
            return;
        };
        let Ok(sig) = crate::delta::Signature::generate(&mut file) else {
            return;
        };
        for tx in &txs {
            let _ = tx.send(sig.clone());
        }
    });
}

/// Compute a delta against a sibling file's signature (cross-file basis):
/// the channel delivers the reference's signature (its delta byproduct or a
/// spawned job), and the delta's Copy ops reference the sibling file on the
/// receiver.
#[expect(clippy::needless_pass_by_value)] // basis_rel moves into the delta
fn compute_cross_delta_job(
    full: &Path,
    basis_rx: std::sync::mpsc::Receiver<Signature>,
    basis_rel: String,
    display_path: String,
    progress: Option<&ProgressFn>,
    verify_hash: bool,
    files_total: u64,
) -> Result<PreparedDelta> {
    let Ok(ref_sig) = basis_rx.recv() else {
        // The reference's transfer was skipped or fell back to a chunked
        // stream without a signature byproduct — the promised "skip with
        // a warning" (the senders drop and the dependent degrades to a
        // whole-file stream instead of aborting the sync).
        tracing::warn!("cross-file basis lost for {display_path} (basis {basis_rel}); streaming the file");
        return Ok(PreparedDelta::ChunkedFallback);
    };
    let file = std::fs::File::open(full).map_err(Error::Io)?;
    let len = file.metadata().map_err(Error::Io)?.len();
    let mut source: Box<dyn std::io::Read> = match progress {
        Some(report) => Box::new(crate::sync::executor::ProgressStream::new(
            file,
            display_path,
            len,
            files_total,
            Arc::clone(report),
        )),
        None => Box::new(file),
    };
    let mut source_sig = Vec::new();
    match compute_delta_limited(
        &mut source,
        &ref_sig,
        DELTA_LITERAL_CHUNK_LIMIT,
        verify_hash,
        Some(&mut source_sig),
    )
    .map_err(Error::from)
    {
        Ok(delta) => {
            let source_signature = (!source_sig.is_empty()).then_some(Signature {
                file_size: delta.source_size,
                chunks: source_sig,
            });
            Ok(PreparedDelta::Delta(delta, source_signature, Some(basis_rel)))
        }
        Err(Error::Delta(DeltaError::LiteralBudgetExceeded { .. })) => {
            Ok(PreparedDelta::ChunkedFallback)
        }
        Err(e) => Err(e),
    }
}

/// The on-disk path of a task's source: entries pulled in through
/// `--follow-links` recursion carry an explicit source path outside the scan
/// root; everything else resolves under `source_root` by its relative path.
fn task_source(source_root: &Path, task: &SyncTask) -> PathBuf {
    file_source(source_root, task.source_path.as_deref(), &task.relative_path)
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Build the pairing indices from `prior` (plan order), mirroring the
    /// caller's loop.
    fn index(prior: &[(&str, u64)]) -> (HashMap<StemKey, Vec<StemEntry>>, HashMap<StemKey, Vec<StemEntry>>) {
        let mut by_stem: HashMap<StemKey, Vec<StemEntry>> = HashMap::new();
        let mut extends: HashMap<StemKey, Vec<StemEntry>> = HashMap::new();
        for (rel, size) in prior {
            register_basis_entry(&mut by_stem, &mut extends, rel, *size);
        }
        (by_stem, extends)
    }

    #[test]
    fn cross_basis_matches_sibling_files() {
        // Sizes at or above CROSS_BASIS_MIN so the entries register.
        let prior = [
            ("a/1.iso", 1_000_000),
            ("a/2.bin", 1_000_000),
            ("b/1.iso", 1_000_000),
        ];
        let (by_stem, extends) = index(&prior);
        // Same dir + stem prefix + close size: pairs.
        assert_eq!(
            indexed_candidate(&by_stem, &extends, "a/1.1.iso", 1_100_000),
            Some(("a/1.iso".to_string(), 1_000_000))
        );
        // Different directory: no.
        assert_eq!(indexed_candidate(&by_stem, &extends, "c/1.1.iso", 1_100_000), None);
        // Different extension: no.
        assert_eq!(indexed_candidate(&by_stem, &extends, "a/1.1.bin", 1_100_000), None);
        // Size ratio beyond 2x: no.
        assert_eq!(indexed_candidate(&by_stem, &extends, "a/1.1.iso", 3_000_000), None);
        // Identical stem (not a proper prefix): no.
        assert_eq!(indexed_candidate(&by_stem, &extends, "a/1.iso", 1_000_000), None);
        // Closest size wins.
        let prior2 = [("a/1.iso", 1_000_000), ("a/1.0.iso", 1_200_000)];
        let (by_stem, extends) = index(&prior2);
        assert_eq!(
            indexed_candidate(&by_stem, &extends, "a/1.1.iso", 1_100_000),
            Some(("a/1.iso".to_string(), 1_000_000))
        );
    }

    #[test]
    fn cross_basis_inverted_dotted_stem() {
        // "1.1.iso" sorts before "1.iso" (the dotted name wins at the
        // first differing char: '1' < 'i'), so the *prefix* stem arrives
        // later — the inverted index is the only way to find the earlier
        // extending entry as its basis.
        let (by_stem, extends) = index(&[("a/1.1.iso", 1_200_000)]);
        assert_eq!(
            indexed_candidate(&by_stem, &extends, "a/1.iso", 1_000_000),
            Some(("a/1.1.iso".to_string(), 1_200_000))
        );
        // Unrelated stems never pair.
        let (by_stem, extends) = index(&[("a/2.bin", 1_200_000)]);
        assert_eq!(indexed_candidate(&by_stem, &extends, "a/1.iso", 1_000_000), None);
    }

    #[test]
    fn basis_entries_below_half_min_are_not_registered() {
        // A 500 KiB entry can never be a basis for a >= 1 MiB candidate
        // (2x ratio), so it must not appear in either index.
        let mut by_stem: HashMap<StemKey, Vec<StemEntry>> = HashMap::new();
        let mut extends: HashMap<StemKey, Vec<StemEntry>> = HashMap::new();
        register_basis_entry(&mut by_stem, &mut extends, "a/x.bin", CROSS_BASIS_MIN / 2 - 1);
        assert!(by_stem.is_empty() && extends.is_empty());
        register_basis_entry(&mut by_stem, &mut extends, "a/x.bin", CROSS_BASIS_MIN / 2);
        assert_eq!(by_stem.len(), 1);
    }

    #[test]
    fn delete_protection_covers_skipped_subtrees() {
        let skipped: std::collections::HashSet<&str> =
            ["extdir", "a/b", "x/y/z"].into_iter().collect();
        // The skipped path itself and anything under it are protected.
        assert!(is_under_skipped("extdir", &skipped));
        assert!(is_under_skipped("extdir/f", &skipped));
        assert!(is_under_skipped("a/b", &skipped));
        assert!(is_under_skipped("a/b/c/d", &skipped));
        // A sibling or a name that merely *starts with* the skipped path
        // (no component boundary) is not.
        assert!(!is_under_skipped("extdir2", &skipped));
        assert!(!is_under_skipped("a/bc", &skipped));
        assert!(!is_under_skipped("x/y", &skipped));
        assert!(!is_under_skipped("unrelated", &skipped));
        assert!(!is_under_skipped("", &skipped));
    }
}
