//! Frame codec: length-prefixed wire format with optional lz4 compression.
//!
//! Wire layout: `[4-byte big-endian length][payload]`. The high bit of the
//! length doubles as a compression flag: when set, the payload is
//! lz4-compressed postcard and must be decompressed before decoding. This
//! avoids a nested `Frame::Compressed` envelope and a second serialization
//! pass.
//!
//! The codec is transport-agnostic: it reads and writes over any
//! `tokio::io::AsyncRead`/`AsyncWrite` byte stream (the ssh stdio channel
//! today, a russh stream for a mobile GUI tomorrow). It has no knowledge of
//! connections — that lives in `crate::transport`.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::Frame;
use super::error::{ProtocolError, Result};

/// Maximum payload length. Two flag bits of the length prefix are spoken for:
/// bit 31 marks a compressed payload, bit 30 marks a chunked-stream frame
/// (see [`send_chunk_frame`]), so lengths are 30-bit.
const MAX_LEN: u32 = 0x3FFF_FFFF;

/// Bit 30 of the length prefix: a frame with a custom raw layout — no
/// postcard envelope. The first payload byte is a tag selecting the layout:
/// [`CHUNK_TAG`] = chunked-stream (`[file_id: u64 LE][raw chunk bytes]`),
/// [`BATCH_TAG`] = small-file batch (`[count: u32 LE]` + per-file records).
/// The bulk paths use this to skip a serialization pass on the sender and a
/// decode copy on the receiver (the payload buffer becomes the frame's
/// data). Never combined with the compression flag.
const CHUNK_FLAG: u32 = 1 << 30;

/// Raw-layout tag: a chunked-stream frame — `[file_id: u64 LE][data]`.
const CHUNK_TAG: u8 = 0;

/// Raw-layout tag: a small-file batch frame — `[count: u32 LE]` then per
/// file `[file_id: u64 LE][path_len: u32 LE][path][data_len: u64 LE]
/// [checksum: u8 flag + 32 bytes when set][data]` — the data written
/// straight from the sender's buffers, read straight into the receiver's.
const BATCH_TAG: u8 = 1;

/// Serialize a frame into a length-prefixed wire buffer, compressing the
/// payload when `compress` is set and the payload exceeds `threshold` bytes.
///
/// # Errors
///
/// Returns an error if serialization fails.
pub fn encode(frame: &Frame, compress: bool, threshold: usize) -> Result<Vec<u8>> {
    let data = postcard::to_allocvec(frame)
        .map_err(|e| ProtocolError::Protocol(format!("Failed to serialize frame: {e}")))?;

    let (payload, flag): (Vec<u8>, u32) = if compress && data.len() > threshold {
        (lz4_flex::block::compress_prepend_size(&data), 1 << 31)
    } else {
        (data, 0)
    };

    if payload.len() > MAX_LEN as usize {
        return Err(ProtocolError::Protocol(
            "Frame exceeds maximum length".to_string(),
        ));
    }
    // Bounded by the MAX_LEN guard above, so the cast cannot truncate.
    #[expect(clippy::cast_possible_truncation)]
    let len = (payload.len() as u32) | flag;

    let mut wire = Vec::with_capacity(4 + payload.len());
    wire.extend_from_slice(&len.to_be_bytes());
    wire.extend_from_slice(&payload);
    Ok(wire)
}

/// Decode a frame from its length prefix and payload (already read off the
/// wire by the caller, which knows the exact payload size).
///
/// The uncompressed path deserializes directly from the borrowed payload —
/// no intermediate copy. Only the compressed path allocates (the
/// decompressed buffer must outlive `postcard::from_bytes`).
///
/// # Errors
///
/// Returns an error if decompression or deserialization fails.
pub fn decode(prefix: [u8; 4], payload: &[u8]) -> Result<Frame> {
    let len = u32::from_be_bytes(prefix);
    if len & CHUNK_FLAG != 0 {
        return Err(ProtocolError::Protocol(
            "chunk frame reached the postcard decode path".to_string(),
        ));
    }
    if len & (1 << 31) != 0 {
        let data = lz4_flex::block::decompress_size_prepended(payload)
            .map_err(|e| ProtocolError::Protocol(format!("Failed to decompress frame: {e}")))?;
        postcard::from_bytes(&data)
            .map_err(|e| ProtocolError::Protocol(format!("Failed to deserialize frame: {e}")))
    } else {
        postcard::from_bytes(payload)
            .map_err(|e| ProtocolError::Protocol(format!("Failed to deserialize frame: {e}")))
    }
}

/// Send a chunked-stream frame: `[4-byte length|CHUNK_FLAG][CHUNK_TAG]
/// [file_id: u64 LE][data]`. The raw bytes go straight from the caller's
/// buffer to the wire — no postcard serialization pass (the chunked path
/// carries bulk data). Uncompressed by design (chunk payloads are arbitrary
/// bytes).
///
/// # Errors
///
/// Returns an error on serialization or write failure.
pub async fn send_chunk_frame<W: AsyncWrite + Unpin>(
    stream: &mut W,
    file_id: u64,
    data: &[u8],
) -> Result<()> {
    let mut wire = Vec::with_capacity(4 + 1 + 8 + data.len());
    chunk_frame_wire(file_id, data, &mut wire)?;
    stream
        .write_all(&wire)
        .await
        .map_err(|e| ProtocolError::Protocol(format!("Failed to write chunk frame: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| ProtocolError::Protocol(format!("Failed to flush chunk frame: {e}")))
}

/// Serialize one chunked-stream frame into `out` without writing — the
/// chunked path accumulates frames and writes them in ~8 MiB batches, so
/// the wire never waits for the pipe/socket round trip per 1 MiB frame
/// (a frame-at-a-time `write_all` keeps only the pipe + socket buffer in
/// flight, capping a real-network transfer at in-flight / RTT — measured
/// ~100 MB/s vs rsync's 650 MB/s over the same link).
///
/// # Errors
///
/// Returns an error if the frame exceeds the maximum length.
pub fn chunk_frame_wire(file_id: u64, data: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let total = 1 + 8 + data.len();
    if total > MAX_LEN as usize {
        return Err(ProtocolError::Protocol(
            "Chunk frame exceeds maximum length".to_string(),
        ));
    }
    // Bounded by the MAX_LEN guard above, so the cast cannot truncate.
    #[expect(clippy::cast_possible_truncation)]
    let framed_len = (total as u32) | CHUNK_FLAG;
    out.extend_from_slice(&framed_len.to_be_bytes());
    out.push(CHUNK_TAG);
    out.extend_from_slice(&file_id.to_le_bytes());
    out.extend_from_slice(data);
    Ok(())
}

/// Send a small-file batch as the zero-copy raw layout (no postcard
/// envelope): `[4-byte length|CHUNK_FLAG][BATCH_TAG][count: u32 LE]` then per
/// file `[file_id: u64 LE][path_len: u32 LE][path][data_len: u64 LE]
/// [checksum flag + 32 bytes when set][data]`. The header and each record's
/// metadata are written from small buffers; the file data is written
/// directly from its own buffer — no full-payload copy (the `-z` path keeps
/// the postcard [`Frame::Batch`] instead, since compression needs the whole
/// payload anyway).
///
/// # Errors
///
/// Returns an error on serialization or write failure.
pub async fn send_batch_raw<W: AsyncWrite + Unpin>(
    stream: &mut W,
    items: &[crate::protocol::BatchItem],
) -> Result<()> {
    let mut total: u64 = 1 + 4;
    for item in items {
        total += 8 + 4 + item.file_path.len() as u64 + 8 + 1 + item.data.len() as u64;
        if item.checksum.is_some() {
            total += 32;
        }
    }
    if total > u64::from(MAX_LEN) {
        return Err(ProtocolError::Protocol(
            "Batch frame exceeds maximum length".to_string(),
        ));
    }
    let mut head = Vec::with_capacity(4 + 1 + 4);
    // Bounded by the MAX_LEN guard above, so the casts cannot truncate
    // (every record carries ≥ 21 header bytes, so the count is bounded too).
    #[expect(clippy::cast_possible_truncation)]
    head.extend_from_slice(&((total as u32) | CHUNK_FLAG).to_be_bytes());
    head.push(BATCH_TAG);
    #[expect(clippy::cast_possible_truncation)]
    head.extend_from_slice(&(items.len() as u32).to_le_bytes());
    stream
        .write_all(&head)
        .await
        .map_err(|e| ProtocolError::Protocol(format!("Failed to write batch frame: {e}")))?;
    // All the record headers in one buffer (one write instead of one per
    // file — the small-file batch can hold thousands of records), then the
    // data written straight from each file's own buffer. The receiver's
    // BATCH_TAG branch parses the headers first, then the data.
    let header_size = items
        .iter()
        .map(|i| 8 + 4 + i.file_path.len() + 8 + 1 + usize::from(i.checksum.is_some()) * 32)
        .sum::<usize>();
    let mut headers = Vec::with_capacity(header_size);
    for item in items {
        headers.extend_from_slice(&item.file_id.to_le_bytes());
        // Bounded by the wire layout, so the cast cannot truncate.
        #[expect(clippy::cast_possible_truncation)]
        headers.extend_from_slice(&(item.file_path.len() as u32).to_le_bytes());
        headers.extend_from_slice(item.file_path.as_bytes());
        headers.extend_from_slice(&(item.data.len() as u64).to_le_bytes());
        match item.checksum {
            Some(checksum) => {
                headers.push(1);
                headers.extend_from_slice(&checksum);
            }
            None => headers.push(0),
        }
    }
    stream
        .write_all(&headers)
        .await
        .map_err(|e| ProtocolError::Protocol(format!("Failed to write batch frame: {e}")))?;
    for item in items {
        stream
            .write_all(&item.data)
            .await
            .map_err(|e| ProtocolError::Protocol(format!("Failed to write batch frame: {e}")))?;
    }
    stream
        .flush()
        .await
        .map_err(|e| ProtocolError::Protocol(format!("Failed to flush batch frame: {e}")))
}

/// Send a frame (uncompressed) over any writable byte stream.
///
/// Flushes after each frame: frames are complete messages (the receiver reads
/// exact lengths), and the flush guarantees they reach the wire immediately —
/// critical on write sides that buffer internally (e.g. `tokio::io::Stdout`
/// wraps `std::io::Stdout`, which block-buffers when not a terminal).
///
/// # Errors
///
/// Returns an error on serialization or write failure.
pub async fn send_frame<W: AsyncWrite + Unpin>(stream: &mut W, frame: &Frame) -> Result<()> {
    send_frame_compressed(stream, frame, false, usize::MAX).await
}

/// Send a frame over any writable byte stream, optionally compressing it
/// (lz4) to save bandwidth.
///
/// When `compress` is set and the serialized frame exceeds `threshold` bytes,
/// the payload is compressed and the length prefix's high bit is set. Small
/// frames pass through uncompressed to avoid the CPU overhead.
///
/// Flushes after each frame (see [`send_frame`]).
///
/// # Errors
///
/// Returns an error on serialization or write failure.
pub async fn send_frame_compressed<W: AsyncWrite + Unpin>(
    stream: &mut W,
    frame: &Frame,
    compress: bool,
    threshold: usize,
) -> Result<()> {
    let wire = encode(frame, compress, threshold)?;
    stream
        .write_all(&wire)
        .await
        .map_err(|e| ProtocolError::Protocol(format!("Failed to write frame: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| ProtocolError::Protocol(format!("Failed to flush frame: {e}")))
}

/// Read a length-prefixed frame from any readable byte stream, decompressing
/// when flagged. A chunked-stream frame (bit 30) is read straight into its
/// payload buffer — the buffer *becomes* the frame's data, no decode copy.
///
/// # Errors
///
/// Returns an error on read, decompression, or deserialization failure.
pub async fn receive_frame<R: AsyncRead + Unpin>(stream: &mut R) -> Result<Frame> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| ProtocolError::Protocol(format!("Failed to read frame length: {e}")))?;

    let len = u32::from_be_bytes(len_buf);
    if len & CHUNK_FLAG != 0 {
        // Raw-layout frame (no postcard envelope): the first payload byte is
        // the tag. The payload is read straight into the frame's data
        // buffers (zero-copy — the postcard pass and its full-payload copy
        // are skipped).
        let size = (len & MAX_LEN) as usize;
        let mut tag_buf = [0u8; 1];
        stream
            .read_exact(&mut tag_buf)
            .await
            .map_err(|e| ProtocolError::Protocol(format!("Failed to read frame tag: {e}")))?;
        match tag_buf[0] {
            CHUNK_TAG => {
                // Chunked-stream frame: `[file_id: u64 LE][raw bytes]`.
                let data_len = size
                    .checked_sub(1 + 8)
                    .ok_or_else(|| ProtocolError::Protocol("Chunk frame shorter than its header".to_string()))?;
                let mut head = [0u8; 8];
                stream
                    .read_exact(&mut head)
                    .await
                    .map_err(|e| ProtocolError::Protocol(format!("Failed to read chunk header: {e}")))?;
                let file_id = u64::from_le_bytes(head);
                let mut data = vec![0u8; data_len];
                stream
                    .read_exact(&mut data)
                    .await
                    .map_err(|e| ProtocolError::Protocol(format!("Failed to read chunk data: {e}")))?;
                return Ok(Frame::FileChunk { file_id, data });
            }
            BATCH_TAG => {
                // Small-file batch: `[count: u32 LE]`, then a header block —
                // per file `[file_id: u64 LE][path_len: u32 LE][path]
                // [data_len: u64 LE][checksum flag + 32 bytes when set]` —
                // then the data block (each file's bytes in record order).
                // The headers are parsed first, the data read straight into
                // each record's buffer (zero-copy).
                use crate::delta::Delta;
                let mut count_buf = [0u8; 4];
                stream
                    .read_exact(&mut count_buf)
                    .await
                    .map_err(|e| ProtocolError::Protocol(format!("Failed to read batch header: {e}")))?;
                let count = u32::from_le_bytes(count_buf) as usize;
                if count > size {
                    return Err(ProtocolError::Protocol(
                        "Batch frame count exceeds its payload length".to_string(),
                    ));
                }
                let mut recipes = Vec::with_capacity(count);
                let mut data_lens: Vec<usize> = Vec::with_capacity(count);
                // The frame's payload length bounds every record: a corrupt
                // or hostile frame must not trigger a giant allocation
                // before the read fails at the frame's end.
                let mut remaining = size.saturating_sub(1 + 4);
                for _ in 0..count {
                    let mut head = [0u8; 8];
                    stream
                        .read_exact(&mut head)
                        .await
                        .map_err(|e| ProtocolError::Protocol(format!("Failed to read batch header: {e}")))?;
                    let file_id = u64::from_le_bytes(head);
                    let mut path_len_buf = [0u8; 4];
                    stream
                        .read_exact(&mut path_len_buf)
                        .await
                        .map_err(|e| ProtocolError::Protocol(format!("Failed to read batch header: {e}")))?;
                    let path_len = u32::from_le_bytes(path_len_buf) as usize;
                    if path_len > remaining {
                        return Err(ProtocolError::Protocol(
                            "Batch record path exceeds the frame length".to_string(),
                        ));
                    }
                    remaining -= path_len;
                    let mut path_bytes = vec![0u8; path_len];
                    stream
                        .read_exact(&mut path_bytes)
                        .await
                        .map_err(|e| ProtocolError::Protocol(format!("Failed to read batch header: {e}")))?;
                    let file_path = String::from_utf8(path_bytes).map_err(|e| {
                        ProtocolError::Protocol(format!("Invalid batch path: {e}"))
                    })?;
                    let mut size_buf = [0u8; 8];
                    stream
                        .read_exact(&mut size_buf)
                        .await
                        .map_err(|e| ProtocolError::Protocol(format!("Failed to read batch header: {e}")))?;
                    let data_len = u64::from_le_bytes(size_buf);
                    // Reject before the truncating cast — the frame's
                    // payload length bounds every record (the `remaining`
                    // budget), so a compliant value fits usize.
                    if data_len > remaining as u64 {
                        return Err(ProtocolError::Protocol(
                            "Batch record data exceeds the frame length".to_string(),
                        ));
                    }
                    #[expect(clippy::cast_possible_truncation, reason = "bounded by the remaining-budget check above")]
                    let data_len = data_len as usize;
                    remaining -= data_len;
                    let mut flag_buf = [0u8; 1];
                    stream
                        .read_exact(&mut flag_buf)
                        .await
                        .map_err(|e| ProtocolError::Protocol(format!("Failed to read batch header: {e}")))?;
                    let checksum = match flag_buf[0] {
                        0 => None,
                        1 => {
                            let mut checksum = [0u8; 32];
                            stream
                                .read_exact(&mut checksum)
                                .await
                                .map_err(|e| ProtocolError::Protocol(format!("Failed to read batch header: {e}")))?;
                            Some(checksum)
                        }
                        other => {
                            return Err(ProtocolError::Protocol(format!(
                                "Invalid batch checksum flag: {other}"
                            )))
                        }
                    };
                    let mut delta = Delta::new(data_len as u64, 0);
                    delta.checksum = checksum;
                    data_lens.push(data_len);
                    recipes.push(crate::protocol::BatchFile {
                        file_id,
                        file_path,
                        delta,
                    });
                }
                // The data block: each file's bytes become its literal op.
                for (recipe, data_len) in recipes.iter_mut().zip(data_lens) {
                    let mut data = vec![0u8; data_len];
                    stream
                        .read_exact(&mut data)
                        .await
                        .map_err(|e| ProtocolError::Protocol(format!("Failed to read batch data: {e}")))?;
                    recipe.delta.push_literal_owned(data);
                }
                return Ok(Frame::Batch { recipes });
            }
            other => {
                return Err(ProtocolError::Protocol(format!(
                    "Unknown raw-layout frame tag: {other}"
                )))
            }
        }
    }

    let size = (len & MAX_LEN) as usize;

    let mut data = vec![0u8; size];
    stream
        .read_exact(&mut data)
        .await
        .map_err(|e| ProtocolError::Protocol(format!("Failed to read frame data: {e}")))?;

    decode(len_buf, &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::Delta;
    use crate::protocol::{BatchFile, BatchItem, BUILD_FINGERPRINT};

    fn small_frame() -> Frame {
        Frame::Hello {
            fingerprint: BUILD_FINGERPRINT.to_string(),
        }
    }

    fn big_frame() -> Frame {
        let mut delta = Delta::new(1024 * 1024, 0);
        delta.push_literal(&vec![0x42u8; 1024 * 1024]);
        Frame::Batch {
            recipes: vec![BatchFile {
                file_id: 1,
                file_path: "a.txt".into(),
                delta,
            }],
        }
    }

    #[test]
    fn wire_roundtrip_uncompressed() {
        let frame = small_frame();
        let wire = encode(&frame, false, 0).unwrap();
        let (prefix, payload) = wire.split_at(4);
        assert_eq!(u32::from_be_bytes(prefix.try_into().unwrap()) >> 31, 0);
        let decoded = decode(prefix.try_into().unwrap(), payload).unwrap();
        assert_eq!(format!("{frame:?}"), format!("{decoded:?}"));
    }

    #[test]
    fn wire_roundtrip_compressed() {
        let frame = big_frame();
        let wire = encode(&frame, true, 1024).unwrap();
        let (prefix, payload) = wire.split_at(4);
        // High bit marks the compressed payload.
        assert_ne!(u32::from_be_bytes(prefix.try_into().unwrap()) >> 31, 0);
        let decoded = decode(prefix.try_into().unwrap(), payload).unwrap();
        match decoded {
            Frame::Batch { recipes } => {
                assert_eq!(recipes.len(), 1);
                assert_eq!(recipes[0].file_path, "a.txt");
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn small_frames_bypass_compression() {
        let frame = small_frame();
        let wire = encode(&frame, true, 1024).unwrap();
        let prefix = u32::from_be_bytes(wire[0..4].try_into().unwrap());
        assert_eq!(prefix >> 31, 0);
    }

    #[test]
    fn compression_shrinks_repetitive_payload() {
        let frame = big_frame();
        let plain = encode(&frame, false, 0).unwrap();
        let compressed = encode(&frame, true, 1024).unwrap();
        assert!(
            compressed.len() < plain.len(),
            "compression should shrink repetitive data ({} vs {})",
            compressed.len(),
            plain.len()
        );
    }

    #[tokio::test]
    async fn codec_works_over_any_byte_stream() {
        // The codec must not care what transport the streams come from: a
        // plain in-memory duplex pair round-trips frames.
        use tokio::io::duplex;
        let (mut a, mut b) = duplex(1024 * 1024);
        let frame = big_frame();
        let writer = tokio::spawn(async move {
            send_frame_compressed(&mut a, &frame, true, 1024)
                .await
                .unwrap();
        });
        let got = receive_frame(&mut b).await.unwrap();
        writer.await.unwrap();
        match got {
            Frame::Batch { recipes } => {
                assert_eq!(recipes.len(), 1);
                assert_eq!(recipes[0].file_path, "a.txt");
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn batch_raw_layout_roundtrip() {
        // The zero-copy batch layout (bit 30 + BATCH_TAG) must round-trip:
        // the receiver's data buffers become the literal ops' content — same
        // bytes, no postcard envelope, checksum preserved.
        use tokio::io::duplex;
        let (mut a, mut b) = duplex(4 * 1024 * 1024);
        let items = vec![
            BatchItem {
                file_id: 7,
                file_path: "sub/a.txt".into(),
                data: vec![0x11u8; 4096],
                checksum: Some([0xAA; 32]),
            },
            BatchItem {
                file_id: 8,
                file_path: "b.bin".into(),
                data: vec![0x22u8; 8192],
                checksum: None,
            },
        ];
        let sent = items;
        let writer = tokio::spawn(async move {
            send_batch_raw(&mut a, &sent).await.unwrap();
        });
        let got = receive_frame(&mut b).await.unwrap();
        writer.await.unwrap();
        match got {
            Frame::Batch { recipes } => {
                assert_eq!(recipes.len(), 2);
                assert_eq!(recipes[0].file_id, 7);
                assert_eq!(recipes[0].file_path, "sub/a.txt");
                assert_eq!(recipes[0].delta.source_size, 4096);
                assert_eq!(recipes[0].delta.checksum, Some([0xAA; 32]));
                assert_eq!(
                    recipes[0].delta.ops,
                    vec![crate::delta::DeltaOp::Literal(vec![0x11u8; 4096])]
                );
                assert_eq!(recipes[1].file_id, 8);
                assert_eq!(recipes[1].file_path, "b.bin");
                assert_eq!(recipes[1].delta.checksum, None);
                assert_eq!(
                    recipes[1].delta.ops,
                    vec![crate::delta::DeltaOp::Literal(vec![0x22u8; 8192])]
                );
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn batch_raw_layout_rejects_unknown_tag() {
        // An unknown raw-layout tag must be a protocol error, not a hang or a
        // misparse (the fingerprint versioning means this only guards bugs).
        let len = 1u32 | CHUNK_FLAG;
        let mut wire = Vec::new();
        wire.extend_from_slice(&len.to_be_bytes());
        wire.push(0x7F);
        let mut cursor = std::io::Cursor::new(wire);
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap()
            .block_on(async { receive_frame(&mut cursor).await })
            .unwrap_err();
        assert!(err.to_string().contains("Unknown raw-layout frame tag"));
    }

    #[tokio::test]
    async fn chunk_frame_zero_copy_roundtrip() {
        // The chunked-stream layout (`bit 30` prefix) must round-trip: the
        // receiver's payload buffer becomes the frame's data — same bytes,
        // no postcard envelope.
        use tokio::io::duplex;
        let (mut a, mut b) = duplex(2 * 1024 * 1024);
        let data: Vec<u8> = (0..1024 * 1024)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let sent_data = data.clone();
        let writer = tokio::spawn(async move {
            send_chunk_frame(&mut a, 42, &sent_data).await.unwrap();
        });
        let got = receive_frame(&mut b).await.unwrap();
        writer.await.unwrap();
        match got {
            Frame::FileChunk { file_id, data: got } => {
                assert_eq!(file_id, 42);
                assert_eq!(got, data);
            }
            other => panic!("expected FileChunk, got {other:?}"),
        }
    }
}
