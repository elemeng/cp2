//! Signature generation and lookup for content-defined-chunk delta computation.
//!
//! A signature is the chunk list of a basis file: each content-defined chunk
//! (`FastCDC` via `chunkrs`) carries its byte offset, length, and BLAKE3 hash.
//! Matching is exact strong-hash lookup — no weak checksum or byte-sliding is
//! needed, because content-defined boundaries re-sync automatically after
//! edits.
//!
//! Hashing uses BLAKE3, which the `blake3` crate SIMD-accelerates on modern
//! CPUs (SSE2/AVX2/AVX-512/NEON); the `FastCDC` cut decision itself is
//! inherently scalar (a per-byte data-dependent decision), as in every CDC
//! implementation. Adapted from the copia crate (MIT licensed).

use std::collections::HashMap;
use std::io::Read;

use bytes::Bytes;
use chunkrs::{Chunk, ChunkConfig, Chunker};
use serde::{Deserialize, Serialize};

use crate::delta::error::{DeltaError, DeltaResult};

/// `FastCDC` chunking configuration, shared by both peers.
///
/// Must be identical on sender and receiver so content-defined boundaries
/// agree. Defaults: 4 KiB min / 16 KiB avg / 64 KiB max, normalization level 2.
#[must_use]
pub fn chunk_config() -> ChunkConfig {
    ChunkConfig::default()
}

/// Stream read buffer used while chunking.
pub(crate) const READ_CHUNK: usize = 1024 * 1024;

/// Stream a reader through the shared `FastCDC` chunker, calling `emit` for
/// every complete chunk (including the final `finish()` chunk). Returns the
/// total number of bytes read. The single chunking pipeline shared by
/// signature generation and delta computation, so both sides agree on
/// boundaries by construction.
///
/// The scan of buffer N+1 overlaps the hashing of buffer N: `push_scan`
/// returns unhashed chunks and [`Chunker::hash_chunks_background`] hashes
/// them on the chunker's pool while the next buffer is scanned, so the
/// per-batch hash (~200 µs at 1 MiB) leaves the critical path on machines
/// with parallel headroom. The emission order is preserved — a batch is
/// emitted only after its hash completes, in scan order.
pub(crate) fn for_each_chunk<R: Read>(
    reader: &mut R,
    mut emit: impl FnMut(&Chunk) -> DeltaResult<()>,
) -> DeltaResult<u64> {
    let mut chunker = Chunker::new(chunk_config());
    let mut buf = vec![0u8; READ_CHUNK];
    let mut total: u64 = 0;
    let mut in_flight: Option<std::sync::mpsc::Receiver<Vec<Chunk>>> = None;
    loop {
        let mut filled = 0usize;
        while filled < buf.len() {
            let n = reader
                .read(&mut buf[filled..])
                .map_err(|e| DeltaError::Chunking(format!("read error: {e}")))?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        total += filled as u64;
        let (chunks, _) = chunker.push_scan(Bytes::copy_from_slice(&buf[..filled]));
        // The previous batch's hash has had this whole scan to finish; join
        // it (near-instant) and emit in scan order.
        if let Some(rx) = in_flight.take() {
            let prev = rx
                .recv()
                .map_err(|e| DeltaError::Chunking(format!("hash task failed: {e}")))?;
            for chunk in prev {
                emit(&chunk)?;
            }
        }
        if !chunks.is_empty() {
            in_flight = Some(chunker.hash_chunks_background(chunks));
        }
    }
    if let Some(rx) = in_flight.take() {
        let prev = rx
            .recv()
            .map_err(|e| DeltaError::Chunking(format!("hash task failed: {e}")))?;
        for chunk in prev {
            emit(&chunk)?;
        }
    }
    if let Some(chunk) = chunker.finish() {
        emit(&chunk)?;
    }
    Ok(total)
}

/// The BLAKE3 hash of a chunk, or an error when chunk hashing is disabled.
pub(crate) fn chunk_hash(chunk: &Chunk) -> DeltaResult<[u8; 32]> {
    chunk
        .hash()
        .map(|h| *h.as_bytes())
        .ok_or_else(|| DeltaError::Chunking("chunk missing hash (hashing disabled?)".to_string()))
}

/// Signature for a single chunk (CDC) or fixed block (rollsum engine) of
/// the basis file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkSignature {
    /// Byte offset of the chunk in the basis file.
    pub offset: u64,
    /// Length of the chunk.
    pub len: u32,
    /// rsync-style weak rolling checksum — `Some` only for the fixed-block
    /// (rollsum) engine, where the sender slides byte-by-byte and uses it as
    /// the hash-table filter before the strong hash.
    pub weak: Option<u32>,
    /// Strong cryptographic hash (BLAKE3) of the chunk contents.
    pub strong_hash: [u8; 32],
}

impl ChunkSignature {
    /// Build a signature from already-computed parts (`weak` is always
    /// `None` here — the fixed-block rollsum engine sets it separately).
    pub(crate) fn from_parts(offset: u64, len: u32, strong_hash: [u8; 32]) -> Self {
        Self {
            offset,
            len,
            weak: None,
            strong_hash,
        }
    }

    /// Build a signature from a chunk emitted by `chunkrs`.
    fn from_chunk(chunk: &Chunk) -> DeltaResult<Self> {
        let strong_hash = chunk_hash(chunk)?;
        let len = u32::try_from(chunk.len())
            .map_err(|_| DeltaError::Chunking(format!("chunk too large: {}", chunk.len())))?;
        Ok(Self::from_parts(chunk.start(), len, strong_hash))
    }
}

/// Complete signature of a basis file: its content-defined chunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Total size of the basis file in bytes.
    pub file_size: u64,
    /// Content-defined chunks of the basis file.
    pub chunks: Vec<ChunkSignature>,
}

impl Signature {
    /// Create a new (empty) signature for a basis of `file_size` bytes.
    #[must_use]
    pub const fn new(file_size: u64) -> Self {
        Self {
            file_size,
            chunks: Vec::new(),
        }
    }

    /// Generate a signature from a reader, streaming content-defined chunks.
    ///
    /// Reads the reader incrementally; memory use is bounded by the chunk
    /// config's max size plus the read buffer.
    ///
    /// # Errors
    ///
    /// Returns an error if reading fails.
    pub fn generate<R: Read>(reader: &mut R) -> DeltaResult<Self> {
        let mut chunks = Vec::new();
        let file_size = for_each_chunk(reader, |chunk| {
            chunks.push(ChunkSignature::from_chunk(chunk)?);
            Ok(())
        })?;
        Ok(Self { file_size, chunks })
    }

    /// Generate a fixed-block signature (the rollsum engine): the basis is
    /// split into `block_size`-byte blocks, each carrying the weak rolling
    /// checksum and the strong BLAKE3. `block_size` is a pure function of
    /// the file size ([`crate::delta::rollsum::block_size`]), so both peers
    /// agree without any configuration.
    ///
    /// Reads through a large buffer (rsync's generator streams the same
    /// per-block `get_checksum1` + strong-hash pair over an mmap); the weak
    /// checksum uses the 4-byte-unrolled init, and the final short block is
    /// included as a normal entry so the sender's tail window can match it.
    ///
    /// The weak scan and the strong hash are independent per block, so the
    /// hashes of one 16 MiB batch run on a background worker thread while
    /// the next batch is scanned — the phase costs the max of the two
    /// passes instead of their sum (~0.25 s vs ~0.39 s on 512 MiB). The
    /// worker is single-threaded and FIFO, so the strong hashes come back
    /// in submission order; a batch boundary waits at most one block's
    /// hash (~6 µs per 23 KiB block at ~2.9 GB/s).
    ///
    /// # Errors
    ///
    /// Returns an error if reading fails or the hash worker cannot be
    /// spawned.
    pub fn generate_rollsum<R: Read>(reader: &mut R, block_size: usize) -> DeltaResult<Self> {
        const BUF: usize = 16 * 1024 * 1024;
        let block = block_size.max(1);

        // Background BLAKE3 worker (one thread, FIFO).
        let (block_tx, block_rx) = std::sync::mpsc::channel::<(u64, Vec<u8>)>();
        let (hash_tx, hash_rx) = std::sync::mpsc::channel::<(u64, [u8; 32])>();
        let worker = std::thread::Builder::new()
            .name("cp2-sig-hash".to_string())
            .spawn(move || {
                while let Ok((off, data)) = block_rx.recv() {
                    if hash_tx.send((off, *blake3::hash(&data).as_bytes())).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| DeltaError::Chunking(format!("spawn signature hash worker: {e}")))?;

        // Errors still reap the worker: run the body, then join it.
        let result = (move || -> DeltaResult<Self> {
            let mut chunks = Vec::new();
            let mut src: Vec<u8> = Vec::new();
            let mut offset = 0u64;
            loop {
                // Top the buffer up (blocks are processed out of the prefix;
                // the sub-block remainder is compacted to the front).
                let old = src.len();
                src.resize(old + BUF, 0);
                let mut filled = 0usize;
                while filled < BUF {
                    let n = reader
                        .read(&mut src[old + filled..old + BUF])
                        .map_err(|e| DeltaError::Chunking(format!("read error: {e}")))?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                src.truncate(old + filled);
                if filled == 0 {
                    break;
                }
                // Scan the batch: weak in the foreground, strong hashes in
                // the worker. The strong fields are filled below, in
                // submission order.
                let batch_start = chunks.len();
                let mut pos = 0usize;
                while pos + block <= src.len() {
                    let (s1, s2) = crate::delta::rollsum::weak_init(&src[pos..pos + block]);
                    block_tx
                        .send((offset, src[pos..pos + block].to_vec()))
                        .map_err(|e| DeltaError::Chunking(format!("hash worker closed: {e}")))?;
                    chunks.push(ChunkSignature {
                        offset,
                        len: u32::try_from(block)
                            .map_err(|_| DeltaError::Chunking("block too large".to_string()))?,
                        weak: Some(crate::delta::rollsum::weak_value(s1, s2)),
                        strong_hash: [0u8; 32],
                    });
                    offset += block as u64;
                    pos += block;
                }
                for chunk in &mut chunks[batch_start..] {
                    let (off, hash) = hash_rx.recv().map_err(|e| {
                        DeltaError::Chunking(format!("hash worker failed: {e}"))
                    })?;
                    debug_assert_eq!(off, chunk.offset, "hash order must follow submission");
                    chunk.strong_hash = hash;
                }
                if pos > 0 {
                    src.copy_within(pos.., 0);
                    src.truncate(src.len() - pos);
                }
            }
            // The final short block, if any.
            if !src.is_empty() {
                let (s1, s2) = crate::delta::rollsum::weak_init(&src);
                block_tx
                    .send((offset, src.clone()))
                    .map_err(|e| DeltaError::Chunking(format!("hash worker closed: {e}")))?;
                chunks.push(ChunkSignature {
                    offset,
                    len: u32::try_from(src.len())
                        .map_err(|_| DeltaError::Chunking("block too large".to_string()))?,
                    weak: Some(crate::delta::rollsum::weak_value(s1, s2)),
                    strong_hash: [0u8; 32],
                });
                let (_, hash) = hash_rx
                    .recv()
                    .map_err(|e| DeltaError::Chunking(format!("hash worker failed: {e}")))?;
                if let Some(last) = chunks.last_mut() {
                    last.strong_hash = hash;
                }
                offset += src.len() as u64;
            }
            let file_size = offset; // authoritative: blocks tile the stream contiguously
            Ok(Self { file_size, chunks })
        })();

        // The body closure owns `block_tx` and drops it on every exit path
        // (Ok or Err), which disconnects the worker; join reaps it.
        let _ = worker.join();
        result
    }
}

/// Efficient lookup table: chunk strong hash → chunk.
///
/// A single exact-lookup map; BLAKE3 collision probability is negligible, so
/// no weak/strong two-level index is needed (unlike fixed-block rsync, which
/// must slide byte-by-byte). The table borrows the signature instead of
/// copying its chunk vector, so building one is O(chunks) hashing with no
/// heap copy of the chunk list.
#[derive(Debug, Clone)]
pub(crate) struct SignatureTable<'a> {
    /// First chunk index per strong hash.
    by_hash: HashMap<[u8; 32], usize>,
    /// Full signature data.
    signature: &'a Signature,
}

impl<'a> SignatureTable<'a> {
    /// Build a signature table from a signature.
    #[must_use]
    pub fn from_signature(signature: &'a Signature) -> Self {
        let mut by_hash = HashMap::with_capacity(signature.chunks.len());
        for (i, chunk) in signature.chunks.iter().enumerate() {
            by_hash.entry(chunk.strong_hash).or_insert(i);
        }
        Self { by_hash, signature }
    }

    /// Find the basis chunk with the given strong hash, if any.
    #[must_use]
    pub fn find(&self, strong: &[u8; 32]) -> Option<&ChunkSignature> {
        self.by_hash.get(strong).map(|&i| &self.signature.chunks[i])
    }

    /// Check if the table has no chunks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.signature.chunks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn signature_generate_empty() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let sig = Signature::generate(&mut cursor).unwrap();
        assert_eq!(sig.file_size, 0);
        assert!(sig.chunks.is_empty());
    }

    #[test]
    fn signature_chunks_cover_file() {
        let data = vec![42u8; 300_000];
        let mut cursor = Cursor::new(data);
        let sig = Signature::generate(&mut cursor).unwrap();
        assert_eq!(sig.file_size, 300_000);
        assert!(!sig.chunks.is_empty());
        // Chunks are contiguous and cover the whole file.
        let mut offset = 0u64;
        for chunk in &sig.chunks {
            assert_eq!(chunk.offset, offset);
            offset += u64::from(chunk.len);
        }
        assert_eq!(offset, sig.file_size);
        // Every chunk is at least the config minimum.
        for chunk in &sig.chunks {
            assert!(u64::from(chunk.len) >= 4 * 1024);
        }
    }

    #[test]
    fn signature_serde_roundtrip() {
        let data = vec![1u8; 50_000];
        let mut cursor = Cursor::new(data);
        let original = Signature::generate(&mut cursor).unwrap();
        let serialized = postcard::to_allocvec(&original).unwrap();
        let restored: Signature = postcard::from_bytes(&serialized).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn table_find_match() {
        // The same content chunked twice yields identical boundaries + hashes.
        let data = vec![7u8; 100_000];
        let sig = Signature::generate(&mut Cursor::new(&data)).unwrap();
        let table = SignatureTable::from_signature(&sig);
        let mut chunker = Chunker::new(chunk_config());
        let (chunks, _) = chunker.push(Bytes::from(data));
        let chunks = [chunks, chunker.finish().into_iter().collect()].concat();
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            let hash = chunk.hash().unwrap();
            let found = table.find(hash.as_bytes()).unwrap();
            assert_eq!(found.len as usize, chunk.len());
        }
    }

    #[test]
    fn table_identical_chunks_share_entry() {
        // A file of repeated identical bytes chunks into identical chunks; the
        // table still resolves (first chunk index) and matches.
        let data = vec![0u8; 200_000];
        let sig = Signature::generate(&mut Cursor::new(&data)).unwrap();
        let table = SignatureTable::from_signature(&sig);
        let mut chunker = Chunker::new(chunk_config());
        let (chunks, _) = chunker.push(Bytes::from(data));
        let chunks = [chunks, chunker.finish().into_iter().collect()].concat();
        assert!(
            chunks.len() > 1,
            "repetitive data should produce multiple chunks"
        );
        let hash = chunks[0].hash().unwrap();
        let found = table.find(hash.as_bytes()).unwrap();
        assert_eq!(found.strong_hash, *hash.as_bytes());
    }
}
