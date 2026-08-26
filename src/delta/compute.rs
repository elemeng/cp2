//! Delta computation and patching.
//!
//! Content-defined-chunk (`FastCDC`) delta: [`compute_delta`] chunks the source
//! with the same configuration used to sign the basis, then matches each
//! source chunk's BLAKE3 hash against the basis signature — emitting `Copy`
//! ops for matches and `Literal` bytes otherwise. Because chunk boundaries are
//! content-defined, unchanged regions re-sync automatically after edits (no
//! byte-sliding or rolling checksum needed), so an edit only re-sends the
//! chunks it touches.
//!
//! [`apply_patch`] reconstructs the source from the basis and the delta,
//! verifying the BLAKE3 checksum. Hashing is SIMD-accelerated by `blake3`.
//!
//! Adapted from the copia crate (MIT licensed).

use std::io::{Read, Seek, SeekFrom, Write};

use chunkrs::Chunk;

use crate::delta::error::{DeltaError, DeltaResult};
use crate::delta::rollsum::MAX_CHAIN_LEN;
use crate::delta::ops::{Delta, DeltaOp};
use crate::delta::signature::{
    ChunkSignature,
    READ_CHUNK, Signature, SignatureTable, chunk_hash, for_each_chunk,
};

/// Stream the basis read of one Copy op through this buffer (see
/// [`apply_patch`]): memory stays bounded no matter how large a contiguous
/// copy run gets.
const COPY_STREAM_CHUNK: u32 = 1024 * 1024;

/// Stream the whole source as one literal-runs delta for an empty (or
/// non-rollsum) basis. Bounded in `buf_size` chunks — no `read_to_end` —
/// with the same literal-budget abort as the chunked paths: a basis that
/// matches nothing must not accumulate the whole source as one in-memory
/// literal. Shared by the `FastCDC` ([`compute_delta_limited`]) and rollsum
/// ([`compute_delta_rollsum`]) engines.
fn stream_all_literal<R: Read>(
    source: &mut R,
    buf_size: usize,
    dest_size: u64,
    max_literal: u64,
    verify: bool,
) -> DeltaResult<Delta> {
    let mut hasher = verify.then(blake3::Hasher::new);
    let mut delta = Delta::new(0, dest_size);
    let mut buf = vec![0u8; buf_size];
    let mut literal_bytes: u64 = 0;
    loop {
        let n = source
            .read(&mut buf)
            .map_err(|e| DeltaError::Chunking(format!("read error: {e}")))?;
        if n == 0 {
            break;
        }
        delta.push_literal(&buf[..n]);
        if let Some(h) = &mut hasher {
            h.update(&buf[..n]);
        }
        literal_bytes += n as u64;
        if literal_bytes > max_literal {
            return Err(DeltaError::LiteralBudgetExceeded { limit: max_literal });
        }
    }
    delta.source_size = literal_bytes;
    delta.checksum = hasher.map(|h| *h.finalize().as_bytes());
    Ok(delta)
}

/// Compute a delta between `source` and a basis described by `signature`.
///
/// The source is streamed and chunked with the same `FastCDC` configuration used
/// to sign the basis; every source chunk is looked up by its BLAKE3 hash.
/// Memory use is bounded by the chunk config's max size plus the read buffer.
///
/// `verify` requests the whole-file BLAKE3 checksum ([`Delta::checksum`]),
/// used by the receiver to verify its reconstruction; off by default, since
/// chunk identity matching already covers the content and the checksum is a
/// second full-file hash pass on both ends.
///
/// # Errors
///
/// Returns an error if reading the source fails.
/// Compute a delta between `source` and a basis described by `signature`.
///
/// The source is streamed and chunked with the same `FastCDC` configuration used
/// to sign the basis; every source chunk is looked up by its BLAKE3 hash.
/// Memory use is bounded by the chunk config's max size plus the read buffer.
///
/// `verify` requests the whole-file BLAKE3 checksum ([`Delta::checksum`]),
/// used by the receiver to verify its reconstruction; off by default, since
/// chunk identity matching already covers the content and the checksum is a
/// second full-file hash pass on both ends.
///
/// # Errors
///
/// Returns an error if reading the source fails.
/// The literal payload is bounded by `max_literal` ([`DeltaError::LiteralBudgetExceeded`])
/// — a basis that matches nothing would otherwise accumulate the whole source
/// as one in-memory literal. The caller falls back to a bounded, resumable
/// whole-file stream.
///
/// bytes ([`DeltaError::LiteralBudgetExceeded`]) — a basis that matches
/// nothing would otherwise accumulate the whole source as one in-memory
/// literal. The caller falls back to a bounded, resumable whole-file stream.
///
/// `sig_out` (when provided) collects the source's chunk signature — the
/// per-chunk hashes are already computed for matching, so the signature is
/// a free byproduct. It is exactly the basis signature the new destination
/// content will have, which lets the receiver cache it for the next run.
///
/// # Errors
///
/// Returns an error if reading the source fails or the literal budget is
/// exceeded.
pub fn compute_delta_limited<R: Read>(
    source: &mut R,
    signature: &Signature,
    max_literal: u64,
    verify: bool,
    mut sig_out: Option<&mut Vec<ChunkSignature>>,
) -> DeltaResult<Delta> {
    let table = SignatureTable::from_signature(signature);

    // Empty basis: everything is literal. Streamed in bounded chunks (no
    // `read_to_end`), with the same budget abort as the chunked path.
    if table.is_empty() {
        return stream_all_literal(source, READ_CHUNK, signature.file_size, max_literal, verify);
    }

    let mut hasher = verify.then(blake3::Hasher::new);
    let mut delta = Delta::new(0, signature.file_size);

    let mut literal_bytes: u64 = 0;
    let source_size = for_each_chunk(source, |chunk| {
        let hash = chunk_hash(chunk)?;
        literal_bytes += push_chunk_op(&mut delta, &table, chunk, hash);
        // The emitted chunks cover the whole byte stream contiguously, so
        // hashing them in order is hashing the stream.
        if let Some(h) = &mut hasher {
            h.update(&chunk.data);
        }
        // Free byproduct: the source's chunk signature — the new destination
        // content's basis signature (see `sig_out`).
        if let Some(out) = &mut sig_out {
            let len = u32::try_from(chunk.len())
                .map_err(|_| DeltaError::Chunking("chunk too large".to_string()))?;
            out.push(ChunkSignature::from_parts(chunk.start(), len, hash));
        }
        if literal_bytes > max_literal {
            return Err(DeltaError::LiteralBudgetExceeded { limit: max_literal });
        }
        Ok(())
    })?;

    delta.source_size = source_size;
    delta.checksum = hasher.map(|h| *h.finalize().as_bytes());

    debug_assert_eq!(
        delta.bytes_matched() + delta.bytes_literal(),
        source_size,
        "delta bytes must sum to source size"
    );

    Ok(delta)
}

/// Chunk and hash a whole source file, producing its chunk signature and,
/// when `verify`, the whole-file BLAKE3 (the resulting delta's checksum).
///
/// This is the sender-side half of the delta computation, split from
/// [`compute_delta_from_signatures`] so it can overlap the receiver's basis
/// signing: the two full-file passes otherwise serialize the transfer (the
/// signature needs no basis, and the matching needs no bytes beyond the
/// chunk hashes already computed here).
///
/// # Errors
///
/// Returns an error if reading the source fails.
pub fn sign_source<R: Read>(
    source: &mut R,
    verify: bool,
) -> DeltaResult<(Signature, Option<[u8; 32]>)> {
    let mut hasher = verify.then(blake3::Hasher::new);
    let mut chunks = Vec::new();
    let file_size = for_each_chunk(source, |chunk| {
        let hash = chunk_hash(chunk)?;
        let len = u32::try_from(chunk.len())
            .map_err(|_| DeltaError::Chunking("chunk too large".to_string()))?;
        // The emitted chunks cover the whole stream contiguously, so hashing
        // them in order is hashing the stream.
        if let Some(h) = &mut hasher {
            h.update(&chunk.data);
        }
        chunks.push(ChunkSignature::from_parts(chunk.start(), len, hash));
        Ok(())
    })?;
    Ok((
        Signature {
            file_size,
            chunks,
        },
        hasher.map(|h| *h.finalize().as_bytes()),
    ))
}

/// Match a pre-computed source signature against a basis signature, emitting
/// the delta ops (the matching half of [`sign_source`]).
///
/// Literal bytes are re-read from the source in stream position — one
/// bounded read per literal chunk — so the sender's fill pass touches just
/// the literal bytes instead of a second full-file read, and the op order
/// stays source-ordered (copies and literals interleave; a literal appended
/// late would reconstruct the wrong byte stream). Chunk boundaries come from
/// the same `FastCDC` pipeline as the basis signing, so the resulting ops
/// are identical to a one-pass [`compute_delta_limited`].
///
/// # Errors
///
/// Returns an error if reading the source fails or the literal budget is
/// exceeded.
pub fn compute_delta_from_signatures<R: Read + Seek>(
    source: &mut R,
    basis: &Signature,
    source_sig: &Signature,
    max_literal: u64,
) -> DeltaResult<Delta> {
    let table = SignatureTable::from_signature(basis);
    let mut delta = Delta::new(0, basis.file_size);
    let mut literal_bytes: u64 = 0;
    for chunk in &source_sig.chunks {
        if let Some(sig) = table.find(&chunk.strong_hash) {
            delta.push_copy(sig.offset, sig.len);
        } else {
            literal_bytes += u64::from(chunk.len);
            if literal_bytes > max_literal {
                return Err(DeltaError::LiteralBudgetExceeded { limit: max_literal });
            }
            // Chunks are bounded by the config's max size (64 KiB), so the
            // usize cast is lossless on any real platform.
            source.seek(SeekFrom::Start(chunk.offset)).map_err(|e| {
                DeltaError::Chunking(format!("seek error: {e}"))
            })?;
            let size = usize::try_from(chunk.len)
                .map_err(|_| DeltaError::Chunking("chunk too large".to_string()))?;
            let mut buf = vec![0u8; size];
            source.read_exact(&mut buf).map_err(|e| {
                DeltaError::Chunking(format!("read error: {e}"))
            })?;
            delta.push_literal_owned(buf);
        }
    }
    delta.source_size = source_sig.file_size;

    debug_assert_eq!(
        delta.bytes_matched() + delta.bytes_literal(),
        delta.source_size,
        "delta bytes must sum to source size"
    );

    Ok(delta)
}

/// Emit one op for a source chunk: a `Copy` of the matching basis chunk, or
/// the chunk's bytes as a `Literal`. Returns the number of literal bytes
/// pushed (0 for a copy), so callers can budget the in-memory literal
/// payload.
fn push_chunk_op(
    delta: &mut Delta,
    table: &SignatureTable,
    chunk: &Chunk,
    hash: [u8; 32],
) -> u64 {
    if let Some(sig) = table.find(&hash) {
        delta.push_copy(sig.offset, sig.len);
        0
    } else {
        delta.push_literal(&chunk.data);
        // Chunks are bounded by the config's max size (64 KiB), so the
        // usize→u64 cast is lossless on any real platform.
        u64::try_from(chunk.len()).unwrap_or(u64::MAX)
    }
}

/// Refill granularity for the rollsum scan buffer: 16 MiB keeps refills
/// rare, and the tail compaction per refill copies at most one window.
const ROLLSUM_BUF: usize = 16 * 1024 * 1024;

/// Compute a delta with the rsync-style rollsum engine: the basis signature
/// is a list of fixed-size blocks (weak rolling checksum + strong BLAKE3);
/// the source is scanned byte-by-byte with the rolling checksum, and every
/// position whose weak checksum hits the block table is verified with the
/// strong hash before a `Copy` op is emitted. Matching blocks anywhere in
/// the source are found; on a verified match the scan jumps the block
/// length and re-initializes the window, so an edit costs only the edited
/// bytes plus at most one block of re-alignment — deterministic, unlike
/// CDC's probabilistic re-sync.
///
/// The hot path mirrors rsync's `hash_search` (match.c): unmasked `u32`
/// rolling state with the masks deferred to the probe, a flat head+chain
/// hash table (bucket `(s1+s2) & 0xFFFF`, `sum % tablesize` for files big
/// enough to overload 65536 buckets), the strong hash computed at most once
/// per offset, a [`MAX_CHAIN_LEN`] cap on weak-matching candidates, and the
/// EOF tail probed with its own (shrunk) window so a short final block can
/// match.
///
/// Aborts with [`DeltaError::LiteralBudgetExceeded`] once the accumulated
/// literal payload exceeds `max_literal` — the caller then falls back to a
/// bounded, resumable whole-file stream, exactly like the CDC engine.
///
/// # Errors
///
/// Returns an error if reading the source fails or the literal budget is
/// exceeded.
#[expect(clippy::many_single_char_names)] // scan/buffer locals n/p/i/c/t/a/b
pub fn compute_delta_rollsum<R: Read>(
    source: &mut R,
    signature: &Signature,
    max_literal: u64,
    verify: bool,
) -> DeltaResult<Delta> {
    let mut delta = Delta::new(0, signature.file_size);
    // The whole-file checksum exists only when the post-transfer comparison
    // will consume it (the same rule as the CDC path: the default mode
    // carries no whole-file hash).
    let mut hasher = verify.then(blake3::Hasher::new);
    let Some(block) = signature
        .chunks
        .iter()
        .find_map(|c| c.weak.map(|_| c.len as usize))
    else {
        // Empty basis or not a rollsum signature: everything is literal (see
        // `stream_all_literal` — bounded chunks, same budget abort).
        return stream_all_literal(source, ROLLSUM_BUF, signature.file_size, max_literal, verify);
    };

    // Hash table (rsync's `build_hash_table`): a flat head array with an
    // explicit chain array — the signature's chunks are immutable wire data,
    // so the chain lives beside them. A bucket miss costs one load and one
    // compare. Files big enough to overload 65536 buckets get a table sized
    // for ~80% load (rsync's count/8*10+11), rounded to a power of two so
    // the modulo compiles to an AND.
    let count = signature.chunks.len();
    let big = count / 8 * 10 + 11 > 1 << 16;
    let tablesize = (count / 8 * 10 + 11).next_power_of_two();
    let mut head_vec: Vec<u64> = Vec::new();
    // The 512 KiB array is a temporary moved into the Box; its *statically
    // known* length is what lets LLVM prove the probe index (masked to 16
    // bits) in bounds — a boxed slice of runtime length would re-insert the
    // per-byte bounds check.
    #[expect(clippy::large_stack_arrays)]
    let mut head_arr: Box<[u64; 1 << 16]> = Box::new([u64::MAX; 1 << 16]);
    if big {
        head_vec = vec![u64::MAX; tablesize];
    }
    // Packed head entries: weak(32) | idx1(16) | idx2(16) in traditional
    // mode, weak(32) | idx(32) in big mode. The bucket probe is then ONE
    // 8-byte random load: the weak compare (and the chain-continuation
    // decision) are register ops, and the non-inlined walk runs only on a
    // weak match or a deeper chain — the measured miss-path cost of the
    // per-hit `heads`+`chain` loads was ~4x the whole scan. `idx1` is the
    // last-inserted block (LIFO, like rsync); `idx2` was the previous
    // first, i.e. `chain[idx1]` — a 2-entry bucket's continuation is
    // decided without touching the chain array. 16-bit indices suffice:
    // traditional mode holds ≤ 52420 blocks by construction.
    let mut chain = vec![u32::MAX; count];
    for (i, c) in signature.chunks.iter().enumerate() {
        let Some(weak) = c.weak else { continue };
        if big {
            let t = weak as usize % tablesize;
            let old = head_vec[t];
            // `old >> 32` and `i` both fit u32 by construction (range-analyzed).
            let ci = (old >> 32) as u32;
            chain[i] = if old == u64::MAX { u32::MAX } else { ci };
            #[expect(clippy::cast_possible_truncation)]
            let wi = i as u32;
            head_vec[t] = u64::from(weak) | (u64::from(wi) << 32);
        } else {
            let t = ((weak & 0xffff).wrapping_add(weak >> 16) & 0xffff) as usize;
            let old = head_arr[t];
            // `old >> 32` fits u32 (range-analyzed) — no truncation possible.
            let old_first = (old >> 32) as u32 & 0xffff;
            chain[i] = if old_first == 0xffff { u32::MAX } else { old_first };
            #[expect(clippy::cast_possible_truncation)]
            let idx1 = i as u32;
            head_arr[t] =
                u64::from(weak) | (u64::from(idx1) << 32) | (u64::from(old_first) << 48);
        }
    }

    let mut src: Vec<u8> = Vec::new(); // scan buffer, refilled in place
    let mut n = 0usize; // valid bytes in `src`
    let mut p = 0usize; // scan offset within `src`
    let mut eof = false;
    let mut source_size: u64 = 0;
    let mut literal: Vec<u8> = Vec::new();
    let mut literal_bytes: u64 = 0;

    loop {
        // Refill: compact the consumed prefix (at most one window) and top
        // the buffer up to ROLLSUM_BUF.
        while n - p < block && !eof {
            if p > 0 {
                src.copy_within(p..n, 0);
                n -= p;
                p = 0;
                src.truncate(n); // drop the consumed prefix for real
            }
            let old = src.len();
            src.resize(old + ROLLSUM_BUF, 0);
            let mut filled = 0usize;
            while filled < ROLLSUM_BUF {
                let r = source
                    .read(&mut src[old + filled..old + ROLLSUM_BUF])
                    .map_err(|e| DeltaError::Chunking(format!("read error: {e}")))?;
                if r == 0 {
                    break;
                }
                // Every source byte enters `src` exactly once (the refill is
                // the only read site; the compaction only moves bytes), so
                // hashing here is hashing the stream.
                if let Some(h) = &mut hasher {
                    h.update(&src[old + filled..old + filled + r]);
                }
                filled += r;
            }
            src.truncate(old + filled);
            n = src.len();
            if filled == 0 {
                eof = true;
            }
        }
        if n - p < block {
            break; // EOF with a partial tail — handled below
        }
        // Flush the pending literal run (accumulated by previous regions)
        // before the region scan: the scan returns its own run, which the
        // caller continues. The op sequence stays byte-contiguous either way.
        if !literal.is_empty() {
            delta.push_literal(&literal);
            literal.clear();
        }
        let region = &src[p..n];
        let (consumed, region_literal, region_bytes, matched) = scan_region(
            region,
            block,
            &signature.chunks,
            &head_vec,
            &head_arr,
            &chain,
            tablesize,
            big,
            &mut delta,
            max_literal,
        )?;
        p += consumed;
        literal = region_literal;
        literal_bytes += region_bytes;
        // The region's literals are already in the delta's ops (pushed by
        // the scan); the announced source size must cover them too, or the
        // receiver's apply truncates the file to the copy-only size.
        source_size += matched + region_bytes;
    }

    // EOF tail: fewer than `block` bytes remain. Only the window of the last
    // block's length can match (no candidate has any other length) — rsync's
    // `end = len + 1 - last_len` bound; everything else is literal.
    let tail = &src[p..n];
    let last_len = signature.chunks.last().map_or(0, |c| c.len as usize);
    let mut matched = None;
    if tail.len() == last_len && last_len > 0 {
        let (a, b) = crate::delta::rollsum::weak_init(tail);
        let sum = crate::delta::rollsum::weak_value(a, b);
        let e = if big {
            head_vec[sum as usize % tablesize]
        } else {
            head_arr[crate::delta::rollsum::bucket_traditional(a, b)]
        };
        if e != u64::MAX {
            let first = if big {
                (e >> 32) as u32
            } else {
                ((e >> 32) & 0xffff) as u32
            };
            matched = probe_bucket(&signature.chunks, first, &chain, sum, last_len, tail);
        }
    }
    if let Some(m) = matched {
        if !literal.is_empty() {
            delta.push_literal(&literal);
            literal.clear();
        }
        let c = &signature.chunks[m];
        delta.push_copy(c.offset, c.len);
        source_size += u64::from(c.len);
    } else {
        literal.extend_from_slice(tail);
        literal_bytes += tail.len() as u64;
        if literal_bytes > max_literal {
            return Err(DeltaError::LiteralBudgetExceeded { limit: max_literal });
        }
        source_size += tail.len() as u64;
    }
    if !literal.is_empty() {
        delta.push_literal(&literal);
    }

    delta.source_size = source_size;
    delta.checksum = hasher.map(|h| *h.finalize().as_bytes());

    debug_assert_eq!(
        delta.bytes_matched() + delta.bytes_literal(),
        source_size,
        "delta bytes must sum to source size"
    );

    Ok(delta)
}

/// Scan one fully-buffered region: probe every position whose full window
/// fits, roll on a miss, jump the block on a verified match. Emits `Copy`
/// ops (and flushes literal runs) into `delta`, returning the number of
/// bytes consumed from the region — either the whole region (last window
/// probed with no match) or a partial window the caller must refill before
/// resuming (match jump past the buffered end) — along with the accumulated
/// literal run, its byte count, and the matched byte count.
///
/// Kept as its own function so the hot state (`s1`/`s2`/`q`) lives in
/// registers across the loop, and the literal run accumulates in a fixed
/// scratch buffer (flushed to the delta in bulk) instead of a `Vec` — a
/// Vec's ptr/len/cap would round-trip through memory every iteration.
#[allow(clippy::too_many_arguments)]
#[expect(clippy::many_single_char_names)] // hot-loop state: q/st/e/k by design
fn scan_region(
    region: &[u8],
    block: usize,
    chunks: &[crate::delta::signature::ChunkSignature],
    head_vec: &[u64],
    head_arr: &[u64; 1 << 16],
    chain: &[u32],
    tablesize: usize,
    big: bool,
    delta: &mut Delta,
    max_literal: u64,
) -> DeltaResult<(usize, Vec<u8>, u64, u64)> {
    /// Literal run buffer: the run accumulates here and is flushed into the
    /// delta in bulk (a per-byte `Vec::push` round-trips the Vec's metadata
    /// through memory — the measured cost is ~3x on the miss path).
    const SCRATCH: usize = 64 * 1024;
    let mut scratch = vec![0u8; SCRATCH].into_boxed_slice();
    let mut lit = 0usize;
    let mut literal_bytes: u64 = 0;
    let mut matched: u64 = 0;
    let mut q = 0usize;
    let (s1, s2) = crate::delta::rollsum::weak_init(&region[..block]);
    let mut st = crate::delta::rollsum::state_pack(s1, s2);
    'outer: loop {
        // ---- hot scan: probe + roll, no calls anywhere on this path ----
        // (a call in the loop body forces the compiler to spill the
        // loop-carried state around it, which costs ~4x on the miss path)
        // The loop's value is the bucket's head entry on a hit — the cold
        // block below reuses it instead of re-loading (the second load's
        // latency would sit on the mispredicted branch's critical path).
        let e = 'hot: loop {
            let sum = crate::delta::rollsum::state_value(st);
            let e = if big {
                head_vec[sum as usize % tablesize]
            } else {
                head_arr[crate::delta::rollsum::state_bucket(st)]
            };
            if e != u64::MAX {
                break 'hot e; // bucket hit — handled cold below
            }
            // ---- no match: roll and accumulate the literal byte ----
            if q + block >= region.len() {
                // Last full window probed with no match: no input byte to
                // roll, so the window bytes are literal. The tail may not
                // fit the fixed scratch (the run's remainder can exceed
                // SCRATCH - block) — flush the accumulated run first.
                if lit + block > SCRATCH && lit > 0 {
                    delta.push_literal(&scratch[..lit]);
                    lit = 0;
                }
                scratch[lit..lit + block].copy_from_slice(&region[q..]);
                lit += block;
                literal_bytes += block as u64;
                if literal_bytes > max_literal {
                    return Err(DeltaError::LiteralBudgetExceeded { limit: max_literal });
                }
                delta.push_literal(&scratch[..lit]);
                return Ok((region.len(), Vec::new(), literal_bytes, matched));
            }
            let out = region[q];
            let inn = region[q + block];
            #[expect(clippy::cast_possible_truncation)]
            let k = block as u32;
            crate::delta::rollsum::state_roll(&mut st, k, out, inn);
            scratch[lit] = out;
            lit += 1;
            literal_bytes += 1;
            if literal_bytes > max_literal {
                return Err(DeltaError::LiteralBudgetExceeded { limit: max_literal });
            }
            if lit == SCRATCH {
                delta.push_literal(&scratch);
                lit = 0;
            }
            q += 1;
        };
        // ---- cold: one bucket-hit position ----
        // The packed head entry (carried out of the hot loop) holds the
        // first candidate's weak and the chain-continuation decision; the
        // non-inlined walk (strong hash, deeper chains) runs only on a weak
        // match or a deeper chain. The spills the walk's BLAKE3 call forces
        // stay in this block.
        let sum = crate::delta::rollsum::state_value(st);
        {
            #[expect(clippy::cast_possible_truncation)]
            let hweak = e as u32;
            let idx1 = ((e >> 32) & 0xffff) as u32;
            let idx2 = (e >> 48) & 0xffff;
            if (hweak == sum || idx2 != 0xffff)
                && let Some(m) = probe_bucket(
                    chunks,
                    idx1,
                    chain,
                    sum,
                    block,
                    &region[q..q + block],
                )
            {
                // Verified match: flush the accumulated literal, emit the
                // Copy, and jump the block.
                if lit > 0 {
                    delta.push_literal(&scratch[..lit]);
                    lit = 0;
                }
                let c = &chunks[m];
                delta.push_copy(c.offset, c.len);
                matched += u64::from(c.len);
                q += block;
                if q + block > region.len() {
                    return Ok((
                        q,
                        Vec::from(&scratch[..lit]),
                        literal_bytes,
                        matched,
                    )); // the jumped-to window is partial — refill
                }
                let (a, b) = crate::delta::rollsum::weak_init(&region[q..q + block]);
                st = crate::delta::rollsum::state_pack(a, b);
                continue 'outer; // the jumped-to window is probed directly
            }
        }
        // ---- miss at the bucket-hit position: roll one byte, resume ----
        if q + block >= region.len() {
            // Last full window probed with no match: the window bytes are
            // literal (no input byte to roll). The tail may not fit the
            // fixed scratch (the run's remainder can exceed SCRATCH -
            // block) — flush the accumulated run first.
            if lit + block > SCRATCH && lit > 0 {
                delta.push_literal(&scratch[..lit]);
                lit = 0;
            }
            scratch[lit..lit + block].copy_from_slice(&region[q..]);
            lit += block;
            literal_bytes += block as u64;
            if literal_bytes > max_literal {
                return Err(DeltaError::LiteralBudgetExceeded { limit: max_literal });
            }
            delta.push_literal(&scratch[..lit]);
            return Ok((region.len(), Vec::new(), literal_bytes, matched));
        }
        let out = region[q];
        let inn = region[q + block];
        #[expect(clippy::cast_possible_truncation)]
        let k = block as u32;
        crate::delta::rollsum::state_roll(&mut st, k, out, inn);
        scratch[lit] = out;
        lit += 1;
        literal_bytes += 1;
        if literal_bytes > max_literal {
            return Err(DeltaError::LiteralBudgetExceeded { limit: max_literal });
        }
        if lit == SCRATCH {
            delta.push_literal(&scratch);
            lit = 0;
        }
        q += 1;
    }
}

/// Walk a bucket chain for one window, returning the index of a verified
/// match or `None`. The strong hash is computed at most once per offset
/// (rsync's `done_csum2`); the walk gives up after [`MAX_CHAIN_LEN`]
/// weak-matching candidates, sending the data literally instead (rsync
/// issue #217 — identical-block runs would otherwise peg the CPU).
///
/// Deliberately **not** inlined: it contains the BLAKE3 call, and inlined
/// into the byte-scan loop the compiler must preserve the rolling state
/// across that call — spilling the loop-carried registers to the stack on
/// every iteration (store-to-load forwarding stalls on the checksum chain).
#[inline(never)]
fn probe_bucket(
    chunks: &[crate::delta::signature::ChunkSignature],
    mut i: u32,
    chain: &[u32],
    sum: u32,
    k: usize,
    window: &[u8],
) -> Option<usize> {
    let mut chain_len = 0u32;
    let mut strong: Option<[u8; 32]> = None;
    while i != u32::MAX {
        let c = &chunks[i as usize];
        if c.len as usize == k && c.weak == Some(sum) {
            chain_len += 1;
            if chain_len > MAX_CHAIN_LEN {
                return None;
            }
            let h = *strong.get_or_insert_with(|| *blake3::hash(window).as_bytes());
            if h == c.strong_hash {
                return Some(i as usize);
            }
        }
        i = chain[i as usize];
    }
    None
}

/// Apply a delta to a basis file, reconstructing the source.
///
/// Copy ops are streamed through one bounded scratch buffer: a contiguous
/// copy run can cover most of a large file (consecutive matching chunks are
/// merged into a single op), so allocating `len` bytes per op would spike
/// memory to multiples of the file size. The patch is otherwise a pure
/// sequential read/write pass — no random access into the output.
///
/// `hasher`: when provided, every applied byte feeds it and the final digest
/// is compared against `delta.checksum` ([`DeltaError::ChecksumMismatch`] on
/// difference). The caller shares its write-path hasher so the applied bytes
/// are hashed exactly once — a separate verification pass would double the
/// BLAKE3 cost of every apply.
///
/// # Errors
///
/// Returns an error if the delta is invalid, reading fails, or the checksum
/// verification fails.
pub fn apply_patch<R: Read + Seek, W: Write>(
    mut basis: R,
    delta: &Delta,
    mut output: W,
    mut hasher: Option<&mut blake3::Hasher>,
) -> DeltaResult<()> {
    delta.validate()?;

    let mut bytes_written: u64 = 0;
    // One scratch buffer, reused across every Copy op (bounded memory no
    // matter how large the merged copy runs get). `usize` is at least 32
    // bits on every supported platform, so the widening is lossless.
    let mut scratch = vec![0u8; COPY_STREAM_CHUNK as usize];

    for op in &delta.ops {
        match op {
            DeltaOp::Copy { offset, len } => {
                basis
                    .seek(SeekFrom::Start(*offset))
                    .map_err(|e| DeltaError::Patch(format!("seek error: {e}")))?;
                let mut remaining = *len;
                while remaining > 0 {
                    let take = remaining.min(COPY_STREAM_CHUNK);
                    let n = usize::try_from(take)
                        .map_err(|_| DeltaError::Patch("copy stream chunk exceeds usize".to_string()))?;
                    basis
                        .read_exact(&mut scratch[..n])
                        .map_err(|e| DeltaError::Patch(format!("read error: {e}")))?;
                    output
                        .write_all(&scratch[..n])
                        .map_err(|e| DeltaError::Patch(format!("write error: {e}")))?;
                    if let Some(h) = hasher.as_deref_mut() {
                        h.update(&scratch[..n]);
                    }
                    bytes_written += u64::from(take);
                    remaining -= take;
                }
            }
            DeltaOp::Literal(data) => {
                output
                    .write_all(data)
                    .map_err(|e| DeltaError::Patch(format!("write error: {e}")))?;
                if let Some(h) = hasher.as_deref_mut() {
                    h.update(data);
                }
                bytes_written += data.len() as u64;
            }
        }
    }

    if let (Some(expected), Some(h)) = (delta.checksum, hasher) {
        let computed = *h.finalize().as_bytes();
        if computed != expected {
            return Err(DeltaError::ChecksumMismatch {
                expected,
                actual: computed,
            });
        }
    }

    debug_assert_eq!(
        bytes_written, delta.source_size,
        "bytes written must equal source size"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn delta_from_signatures_matches_one_pass() {
        // The split engine (sign_source + compute_delta_from_signatures) must
        // emit the same ops as the one-pass compute_delta_limited. The basis
        // is pseudo-random and 1 MiB (dozens of chunks), so both copy-heavy
        // and literal-heavy layouts interleave — a single-chunk file would
        // not exercise the ordering (the ops must stay source-ordered).
        let mut rng = 0x5EED_u64;
        let mut rand_bytes = |n: usize| {
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                v.push(u8::try_from(rng).unwrap_or(u8::MAX));
            }
            v
        };
        let basis = rand_bytes(1024 * 1024);
        let all_literal = rand_bytes(700 * 1024);
        let cases = [
            // mid-file insertion (tail shifts: chunk boundaries move)
            {
                let mut s = basis.clone();
                s.splice(500 * 1024..500 * 1024, rand_bytes(70000));
                s
            },
            // mid-file overwrite
            {
                let mut s = basis.clone();
                s[300 * 1024..300 * 1024 + 4096].copy_from_slice(&rand_bytes(4096));
                s
            },
            // appended tail
            {
                let mut s = basis.clone();
                s.extend_from_slice(&rand_bytes(123_456));
                s
            },
            // completely different content (all literal)
            all_literal.clone(),
        ];
        for (i, source) in cases.iter().enumerate() {
            let basis_sig = Signature::generate(&mut Cursor::new(basis.as_slice())).unwrap();
            let (src_sig, _) = sign_source(&mut Cursor::new(source.as_slice()), false).unwrap();
            let split =
                compute_delta_from_signatures(&mut Cursor::new(source.as_slice()), &basis_sig, &src_sig, u64::MAX)
                    .unwrap();
            let one_pass =
                compute_delta_limited(&mut Cursor::new(source.as_slice()), &basis_sig, u64::MAX, false, None)
                    .unwrap();

            let mut out_split = Vec::new();
            let mut out_one = Vec::new();
            apply_patch(Cursor::new(&basis), &split, &mut out_split, None).unwrap();
            apply_patch(Cursor::new(&basis), &one_pass, &mut out_one, None).unwrap();
            assert_eq!(out_split, *source, "split engine must reconstruct the source");
            assert_eq!(out_one, *source, "one-pass engine must reconstruct the source");
            assert_eq!(out_split, out_one, "split and one-pass engines must agree");
            // The literal payload is only what changed, not the whole file
            // (the all-literal case legitimately resends everything).
            if i < 3 {
                assert!(
                    split.bytes_literal() < source.len() as u64 / 2,
                    "an edit must not resend the majority of the file ({} literal of {})",
                    split.bytes_literal(),
                    source.len()
                );
            }
        }
    }

    #[test]
    fn delta_identical_files() {
        // Identical content chunks identically: everything matches.
        let data = vec![42u8; 3000];
        let sig = Signature::generate(&mut Cursor::new(&data)).unwrap();
        let delta = compute_delta_limited(&mut Cursor::new(&data), &sig, u64::MAX, true, None).unwrap();
        assert_eq!(delta.bytes_matched(), data.len() as u64);
        assert_eq!(delta.bytes_literal(), 0);
        assert_eq!(
            delta.bytes_matched() + delta.bytes_literal(),
            3000,
            "invariant: matched + literal == source size"
        );
    }

    #[test]
    fn delta_with_insertion() {
        // Several FastCDC chunks: an insertion only perturbs the chunk it
        // touches; later chunks re-sync because boundaries are content-defined.
        // (Pseudo-random data — highly periodic content degenerates to a
        // single chunk under CDC.)
        let basis = pseudo_random(100_000);
        let mut with_insert = basis.clone();
        with_insert.splice(50_000..50_000, b"INSERTED-".iter().copied());

        let sig = Signature::generate(&mut Cursor::new(&basis)).unwrap();
        let delta = compute_delta_limited(&mut Cursor::new(&with_insert), &sig, u64::MAX, true, None).unwrap();

        assert!(delta.bytes_matched() > 0, "unchanged chunks must match");
        assert!(delta.bytes_literal() > 0, "edited chunk must be literal");
        assert_eq!(
            delta.bytes_matched() + delta.bytes_literal(),
            with_insert.len() as u64
        );
    }

    #[test]
    fn delta_all_new_data() {
        let basis = b"old data that is completely different".to_vec();
        let source = b"brand new content entirely unlike the basis".to_vec();

        let sig = Signature::generate(&mut Cursor::new(&basis)).unwrap();
        let delta = compute_delta_limited(&mut Cursor::new(&source), &sig, u64::MAX, true, None).unwrap();

        assert_eq!(delta.bytes_literal(), source.len() as u64);
        assert_eq!(delta.bytes_matched(), 0);
    }

    #[test]
    fn patch_roundtrip() {
        // Multi-chunk data so both Copy and Literal paths are exercised.
        let basis = pseudo_random(100_000);
        let mut source = basis.clone();
        source.splice(50_000..50_000, b"INSERTED-".iter().copied());
        source.splice(20_000..20_005, b"CHANGE".iter().copied());

        let sig = Signature::generate(&mut Cursor::new(&basis)).unwrap();
        let delta = compute_delta_limited(&mut Cursor::new(&source), &sig, u64::MAX, true, None).unwrap();
        assert!(delta.bytes_matched() > 0, "unchanged chunks must match");

        let mut output = Vec::new();
        apply_patch(Cursor::new(&basis), &delta, &mut output, Some(&mut blake3::Hasher::new())).unwrap();
        assert_eq!(output, source);
    }

    #[test]
    fn patch_empty_source() {
        let basis: Vec<u8> = Vec::new();
        let source: Vec<u8> = Vec::new();
        let sig = Signature::generate(&mut Cursor::new(&basis)).unwrap();
        let delta = compute_delta_limited(&mut Cursor::new(&source), &sig, u64::MAX, true, None).unwrap();
        let mut output = Vec::new();
        apply_patch(Cursor::new(&basis), &delta, &mut output, Some(&mut blake3::Hasher::new())).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn patch_checksum_mismatch() {
        let basis = b"some basis content".to_vec();
        let mut delta = Delta::new(100, basis.len() as u64);
        delta.push_copy(0, basis.len().try_into().unwrap());
        // Corrupt checksum.
        delta.checksum = Some([0u8; 32]);

        let mut output = Vec::new();
        let err = apply_patch(Cursor::new(&basis), &delta, &mut output, Some(&mut blake3::Hasher::new())).unwrap_err();
        assert!(matches!(err, DeltaError::ChecksumMismatch { .. }));
    }

    #[test]
    fn patch_invalid_copy_bounds() {
        let basis = vec![0u8; 100];
        let mut delta = Delta::new(200, 100);
        delta.push_copy(0, 200); // exceeds basis_size

        let mut output = Vec::new();
        let err = apply_patch(Cursor::new(&basis), &delta, &mut output, None).unwrap_err();
        assert!(matches!(err, DeltaError::InvalidCopyBounds { .. }));
    }

    /// Deterministic pseudo-random bytes (LCG) — CDC-friendly, unlike highly
    /// periodic content which degenerates to a single chunk.
    fn pseudo_random(len: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(len);
        let mut state = 0x1234_5678u64;
        for _ in 0..len {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            // Keep the low byte of the LCG output's upper bits (intentional
            // truncation of pseudo-random data).
            #[expect(clippy::cast_possible_truncation)]
            let byte = (state >> 33) as u8;
            data.push(byte);
        }
        data
    }

    #[test]
    fn delta_large_streaming_roundtrip() {
        // Large input exercising multiple chunker pushes, with an insertion in
        // the middle.
        let basis = pseudo_random(4 * 1024 * 1024);
        let mut source = basis.clone();
        source.splice(2 * 1024 * 1024..2 * 1024 * 1024, vec![0xEEu8; 16 * 1024]);

        let sig = Signature::generate(&mut Cursor::new(&basis)).unwrap();
        let delta = compute_delta_limited(&mut Cursor::new(&source), &sig, u64::MAX, true, None).unwrap();
        assert_eq!(
            delta.bytes_matched() + delta.bytes_literal(),
            source.len() as u64,
            "invariant: matched + literal == source size"
        );

        let mut output = Vec::new();
        apply_patch(Cursor::new(&basis), &delta, &mut output, Some(&mut blake3::Hasher::new())).unwrap();
        assert_eq!(output, source);
    }

    #[test]
    fn delta_empty_basis_all_literal() {
        // Empty basis signature → the entire source is a literal.
        let source = vec![7u8; 4096];
        let sig = Signature::generate(&mut Cursor::new(Vec::<u8>::new())).unwrap();
        let delta = compute_delta_limited(&mut Cursor::new(&source), &sig, u64::MAX, true, None).unwrap();
        assert_eq!(delta.bytes_literal(), source.len() as u64);
        assert_eq!(delta.bytes_matched(), 0);
    }

    #[test]
    fn delta_literal_budget_aborts_on_useless_basis() {
        // A basis that matches nothing would accumulate the whole source as
        // one in-memory literal; the budget must abort instead.
        let source = pseudo_random(1024 * 1024);
        let basis = pseudo_random(4096); // unrelated content
        let sig = Signature::generate(&mut Cursor::new(&basis)).unwrap();
        let err = compute_delta_limited(&mut Cursor::new(&source), &sig, 64 * 1024, true, None).unwrap_err();
        assert!(matches!(err, DeltaError::LiteralBudgetExceeded { .. }));
    }

    #[test]
    fn delta_empty_basis_respects_budget() {
        // The empty-basis path streams instead of read_to_end, and honors the
        // same budget.
        let source = vec![7u8; 300_000];
        let sig = Signature::generate(&mut Cursor::new(Vec::<u8>::new())).unwrap();
        let err = compute_delta_limited(&mut Cursor::new(&source), &sig, 1000, true, None).unwrap_err();
        assert!(matches!(err, DeltaError::LiteralBudgetExceeded { .. }));
    }

    #[test]
    fn delta_literal_budget_does_not_abort_on_matches() {
        // A useful basis produces only Copy ops: the budget is never crossed.
        let data = pseudo_random(300_000);
        let sig = Signature::generate(&mut Cursor::new(&data)).unwrap();
        let delta = compute_delta_limited(&mut Cursor::new(&data), &sig, 1024, true, None).unwrap();
        assert_eq!(delta.bytes_literal(), 0);
        assert_eq!(delta.bytes_matched(), data.len() as u64);
    }

    #[test]
    fn delta_default_mode_has_no_checksum() {
        // Default mode: no whole-file checksum is computed, and applying
        // without a hasher is a pure reconstruction — no BLAKE3 pass.
        let basis = pseudo_random(100_000);
        let mut source = basis.clone();
        source.splice(50_000..50_000, b"INSERTED-".iter().copied());
        let sig = Signature::generate(&mut Cursor::new(&basis)).unwrap();
        let delta = compute_delta_limited(&mut Cursor::new(&source), &sig, u64::MAX, false, None).unwrap();
        assert!(
            delta.checksum.is_none(),
            "default mode must not carry a whole-file checksum"
        );
        let mut output = Vec::new();
        apply_patch(Cursor::new(&basis), &delta, &mut output, None).unwrap();
        assert_eq!(output, source);

        // Verify mode: the checksum is present and a corrupt one is caught.
        let delta = compute_delta_limited(&mut Cursor::new(&source), &sig, u64::MAX, true, None).unwrap();
        assert!(delta.checksum.is_some(), "verify mode must carry the checksum");
        let mut output = Vec::new();
        let mut h = blake3::Hasher::new();
        apply_patch(Cursor::new(&basis), &delta, &mut output, Some(&mut h)).unwrap();
        assert_eq!(output, source);
    }
    #[test]
    fn delta_edit_cost_is_bounded() {
        // A mid-file random edit must not retransmit the tail: literal bytes
        // stay bounded by the edit size plus re-sync slack (CDC boundaries
        // re-align within a few chunks of the transition). Guards the delta
        // pipeline against a regression to whole-tail retransmission.
        let base = pseudo_random(8 * 1024 * 1024);
        let mid = 4 * 1024 * 1024;
        // Inverted LCG bytes: random-looking, and provably different from
        // the base stream (same-seed pseudo_random would equal base[0..]).
        let edit: Vec<u8> = pseudo_random(1024 * 1024).iter().map(|b| !b).collect();

        // Overwrite 1 MiB in place.
        let mut ov = base.clone();
        ov[mid..mid + edit.len()].copy_from_slice(&edit);
        // Delete 1 MiB.
        let mut del = Vec::with_capacity(base.len() - edit.len());
        del.extend_from_slice(&base[..mid]);
        del.extend_from_slice(&base[mid + edit.len()..]);

        let sig = Signature::generate(&mut Cursor::new(&base)).unwrap();
        for (name, src, bound) in [
            ("overwrite", &ov, 2 * edit.len()),
            ("delete", &del, 4 * 16 * 1024),
        ] {
            let delta = compute_delta_limited(&mut Cursor::new(src), &sig, u64::MAX, false, None).unwrap();
            let literal = delta.bytes_literal();
            assert!(
                literal <= bound as u64,
                "{name}: literal {literal} exceeds bound {bound}"
            );
            // The reconstruction must be byte-exact.
            let mut out = Vec::new();
            let mut h = blake3::Hasher::new();
            apply_patch(Cursor::new(&base), &delta, &mut out, Some(&mut h)).unwrap();
            assert_eq!(out, *src, "{name}: reconstruction differs");
        }
    }
    #[test]
    fn rollsum_roundtrip_with_edits() {
        // Basis and source: 4 MiB, with an overwrite and an insertion.
        let basis = pseudo_random(4 * 1024 * 1024);
        let mut source = basis.clone();
        source.splice(1024 * 1024..1024 * 1024 + 4096, vec![0xEEu8; 4096]);
        for b in &mut source[2 * 1024 * 1024..2 * 1024 * 1024 + 8192] {
            *b ^= 0xFF;
        }

        let block = crate::delta::rollsum::block_size(basis.len() as u64);
        let sig = Signature::generate_rollsum(&mut Cursor::new(&basis), block).unwrap();
        assert!(
            sig.chunks.iter().all(|c| c.weak.is_some()),
            "rollsum signature must carry weak checksums"
        );
        // Blocks tile the basis contiguously.
        let mut expect = 0u64;
        for c in &sig.chunks {
            assert_eq!(c.offset, expect);
            expect += u64::from(c.len);
        }
        assert_eq!(expect, basis.len() as u64);

        let delta = compute_delta_rollsum(&mut Cursor::new(&source), &sig, u64::MAX, false).unwrap();
        let mut out = Vec::new();
        apply_patch(Cursor::new(&basis), &delta, &mut out, None).unwrap();
        assert_eq!(out, source, "rollsum reconstruction must be byte-exact");
    }

    #[test]
    fn rollsum_empty_basis_is_whole_literal() {
        let basis: Vec<u8> = Vec::new();
        let source = pseudo_random(64 * 1024);
        let sig = Signature::generate_rollsum(&mut Cursor::new(&basis), 700).unwrap();
        let delta =
            compute_delta_rollsum(&mut Cursor::new(&source), &sig, u64::MAX, false).unwrap();
        assert_eq!(delta.bytes_literal(), source.len() as u64);
        let mut out = Vec::new();
        apply_patch(Cursor::new(&basis), &delta, &mut out, None).unwrap();
        assert_eq!(out, source);
    }

    #[test]
    fn rollsum_verify_sets_the_whole_file_checksum() {
        // `--rollsum --verify` compares the sender's source hash against the
        // receiver's applied-bytes hash: the rollsum delta must carry the
        // whole-file BLAKE3 exactly when verification is requested — both on
        // the matched path and on the all-literal (empty-basis) path.
        let basis = pseudo_random(2 * 1024 * 1024);
        let block = crate::delta::rollsum::block_size(basis.len() as u64);
        let sig = Signature::generate_rollsum(&mut Cursor::new(&basis), block).unwrap();

        let source = pseudo_random(2 * 1024 * 1024);
        let delta = compute_delta_rollsum(&mut Cursor::new(&source), &sig, u64::MAX, true).unwrap();
        assert_eq!(delta.checksum, Some(*blake3::hash(&source).as_bytes()));
        // Default mode still carries no whole-file hash.
        let delta = compute_delta_rollsum(&mut Cursor::new(&source), &sig, u64::MAX, false).unwrap();
        assert_eq!(delta.checksum, None);

        let empty = Signature::generate_rollsum(&mut Cursor::new(&[][..]), 700).unwrap();
        let delta = compute_delta_rollsum(&mut Cursor::new(&source), &empty, u64::MAX, true).unwrap();
        assert_eq!(delta.checksum, Some(*blake3::hash(&source).as_bytes()));
    }

}

