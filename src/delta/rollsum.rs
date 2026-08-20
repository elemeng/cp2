//! rsync-style rolling checksum (the "rollsum") for fixed-block delta.
//!
//! The weak checksum used by rsync's block-matching: two 16-bit sums over
//! the block, combined into 32 bits, rollable in O(1) per byte:
//!
//! - `s1 = Σ x_i (mod 2^16)`
//! - `s2 = Σ (n − i)·x_i (mod 2^16)` — the first byte has weight `n`, the
//!   last weight 1
//! - `roll`: `s1' = s1 + in − out`; `s2' = s2 + s1' − n·out`
//!
//! The 32-bit value is weak (collisions are expected) — the strong BLAKE3
//! hash of the window is verified before a match is accepted, exactly like
//! rsync. This engine exists on the `rollsum` branch to compare the
//! rsync-style fixed-block delta (deterministic `≤1-block` re-sync, cheap
//! per-byte scan) against `FastCDC` in the same codebase.
//!
//! Performance follows rsync's `match.c`/`generator.c` exactly:
//!
//! - the scan state is **unmasked `u32`** (wrapping arithmetic); the
//!   `& 0xffff` masks are deferred to the probe, so the per-byte roll is two
//!   dependent adds — `s1 = s1 + in − out`, `s2 = s2 + s1' − n·out` (mod
//!   2^32, a ring homomorphism keeps the low 16 bits exact);
//! - a window checksum is computed with the **4-byte unrolled** loop rsync
//!   uses in `get_checksum1` (2 adds per 4 bytes instead of a weighted
//!   multiply per byte) — this is the per-match re-init path;
//! - the probe table is a flat `head` array (rsync's `SUM2HASH2(s1,s2) =
//!   (s1+s2) & 0xFFFF` bucket, `BIG_SUM2HASH` = `sum % tablesize` for files
//!   big enough that the 16-bit table would overload) with a `chain` array,
//!   so a bucket miss costs one load and one compare — no per-bucket `Vec`.

/// rsync's `MAX_CHAIN_LEN`: the number of weak-matching candidates probed at
/// one offset before the bucket is declared pathological and the data is
/// sent literally (disk/VM images contain runs of identical blocks; an
/// unbounded chain would peg the CPU — rsync issue #217).
pub(crate) const MAX_CHAIN_LEN: u32 = 1024;

/// 4-byte-unrolled weak checksum of a window (rsync's `get_checksum1` with
/// `CHAR_OFFSET = 0`).
///
/// Returns the **unmasked** `(s1, s2)` as wrapping `u32`; the per-byte naive
/// form `s1 += b; s2 += s1` is algebraically identical mod 2^32 to the
/// unrolled form `s2 += 4·s1 + 4·b0 + 3·b1 + 2·b2 + b3; s1 += b0+b1+b2+b3`,
/// so the low 16 bits (all the probe uses) are exact. This is the
/// `get_checksum1` re-init rsync performs after every confirmed match —
/// O(block) at ~1 cycle/byte, far cheaper than a weighted-multiply loop.
#[inline]
pub(crate) fn weak_init(window: &[u8]) -> (u32, u32) {
    let mut s1 = 0u32;
    let mut s2 = 0u32;
    let mut i = 0usize;
    while i + 4 <= window.len() {
        let b0 = u32::from(window[i]);
        let b1 = u32::from(window[i + 1]);
        let b2 = u32::from(window[i + 2]);
        let b3 = u32::from(window[i + 3]);
        // s2 += 4·s1 + 4·b0 + 3·b1 + 2·b2 + b3; s1 += b0 + b1 + b2 + b3
        s2 = s2.wrapping_add(
            (s1.wrapping_add(b0) << 2).wrapping_add(3 * b1 + 2 * b2 + b3),
        );
        s1 = s1.wrapping_add(b0 + b1 + b2 + b3);
        i += 4;
    }
    while i < window.len() {
        s1 = s1.wrapping_add(u32::from(window[i]));
        s2 = s2.wrapping_add(s1);
        i += 1;
    }
    (s1, s2)
}

/// The combined 32-bit checksum of a state from [`weak_init`] (masks s2's
/// low 16 bits into the high half — the probe value, and the value stored in
/// the signature).
#[must_use]
#[inline]
pub(crate) fn weak_value(s1: u32, s2: u32) -> u32 {
    (s1 & 0xffff) | (s2 << 16)
}

/// Probe bucket for the traditional 16-bit table (rsync's `SUM2HASH2`):
/// both halves mixed by addition, mod 2^16.
#[must_use]
#[inline]
pub(crate) fn bucket_traditional(s1: u32, s2: u32) -> usize {
    (s1.wrapping_add(s2) & 0xffff) as usize
}

// Packed-state variants for the scan hot loop: both checksum halves in one
// u64 (s1 low, s2 high), so the loop-carried state occupies a single
// register — the compiler otherwise runs out of registers under the probe's
// live range and round-trips one half through the stack every iteration.

/// Pack both halves into one u64.
#[inline]
pub(crate) fn state_pack(s1: u32, s2: u32) -> u64 {
    u64::from(s1) | (u64::from(s2) << 32)
}

/// [`state_pack`] inverse — the probe's combined 32-bit checksum.
#[inline]
pub(crate) fn state_value(st: u64) -> u32 {
    #[expect(clippy::cast_possible_truncation)]
    let v = ((st & 0xffff) | ((st >> 32) << 16)) as u32;
    v
}

/// [`state_pack`] inverse — the traditional bucket index.
#[inline]
pub(crate) fn state_bucket(st: u64) -> usize {
    (st.wrapping_add(st >> 32) & 0xffff) as usize
}

/// Roll one byte on a packed state (see [`state_pack`]).
#[inline]
pub(crate) fn state_roll(st: &mut u64, k: u32, out: u8, inn: u8) {
    // The update is rearranged so the two halves do **not** depend on each
    // other's new value: `s2' = s2 + s1 + in − (n+1)·out` (substituting
    // `s1' = s1 + in − out` into the classic `s2' = s2 + s1' − n·out`
    // leaves only the *old* `s1`). Both chains then advance in parallel —
    // one dependent add per byte instead of two. Mod 2^32 the forms are
    // identical, so the low 16 probe bits are unchanged.
    #[expect(clippy::cast_possible_truncation)]
    let lo = *st as u32;
    let hi = (*st >> 32) as u32;
    let out = u32::from(out);
    let inn = u32::from(inn);
    let lo2 = lo.wrapping_add(inn).wrapping_sub(out);
    let hi2 = hi
        .wrapping_add(lo)
        .wrapping_add(inn)
        .wrapping_sub(k.wrapping_add(1).wrapping_mul(out));
    *st = u64::from(lo2) | (u64::from(hi2) << 32);
}

/// rsync's automatic block size: the square root of the file size rounded
/// down to a multiple of 8, clamped to [700, 32768]. Verified empirically against rsync 3.4.1
/// (32 MiB → 5792, 256 MiB → 16384, 512 MiB → 23168, 1 GiB → 32768).
#[must_use]
pub(crate) fn block_size(file_size: u64) -> usize {
    // Integer square root (Newton), exact for u64 — no float precision loss.
    #[expect(clippy::cast_possible_truncation)]
    let root = isqrt(file_size) as usize;
    let rounded = (root / 8) * 8;
    rounded.clamp(700, 32 * 1024)
}

/// Exact integer square root (floor).
fn isqrt(n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    let mut x = n;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = u64::midpoint(x, n / x);
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Naive reference: recompute both sums from scratch, byte by byte.
    fn naive(window: &[u8]) -> (u32, u32) {
        let mut s1 = 0u32;
        let mut s2 = 0u32;
        #[expect(clippy::cast_possible_truncation)]
        let n = window.len() as u32;
        for (i, &b) in window.iter().enumerate() {
            s1 = s1.wrapping_add(u32::from(b));
            #[expect(clippy::cast_possible_truncation)]
            let w = n.wrapping_sub(i as u32);
            s2 = s2.wrapping_add(w.wrapping_mul(u32::from(b)));
        }
        (s1, s2)
    }

    #[test]
    fn weak_init_matches_naive() {
        // Deterministic pseudo-random data, seeded per length so each window
        // is distinct.
        let data: Vec<u8> = (0..100_000u64)
            .map(|i| (i.wrapping_mul(0x9E37_79B9).rotate_left(17) & 0xFF) as u8)
            .collect();
        for len in [0usize, 1, 2, 3, 4, 5, 7, 8, 12, 63, 64, 700, 23168, 32768] {
            if len > data.len() {
                continue;
            }
            assert_eq!(
                weak_init(&data[..len]),
                naive(&data[..len]),
                "unrolled init diverges at len {len}"
            );
            assert_eq!(
                weak_value(weak_init(&data[..len]).0, weak_init(&data[..len]).1),
                weak_value(naive(&data[..len]).0, naive(&data[..len]).1)
            );
        }
    }

    #[test]
    fn roll_matches_reinit() {
        // The rolling update must reproduce the from-scratch checksum at
        // every position (the invariant the whole scan relies on).
        let data: Vec<u8> = (0..80_000u64)
            .map(|i| (i.wrapping_mul(0x85EB_CA6B).rotate_left(13) & 0xFF) as u8)
            .collect();
        let n = 5792;
        let (s1, s2) = weak_init(&data[..n]);
        let mut st = state_pack(s1, s2);
        for p in 0..data.len() - n {
            let (a, b) = weak_init(&data[p..p + n]);
            assert_eq!(
                state_pack(a, b),
                st,
                "rolling state diverges at position {p}"
            );
            #[expect(clippy::cast_possible_truncation)]
            let k = n as u32;
            state_roll(&mut st, k, data[p], data[p + n]);
        }
        let (a, b) = weak_init(&data[data.len() - n..]);
        assert_eq!(
            state_pack(a, b),
            st,
            "final rolled state diverges"
        );
    }

    #[test]
    fn roll_shrink_matches_reinit() {
        // The tail: as the window shrinks (no input byte), the state must
        // equal the from-scratch checksum of the shorter window.
        let data: Vec<u8> = (0..40_000u64)
            .map(|i| (i.wrapping_mul(0xC2B2_AE35).rotate_left(29) & 0xFF) as u8)
            .collect();
        let mut n = 5792usize;
        let (mut s1, mut s2) = weak_init(&data[..n]);
        for p in 0..data.len() - 1 {
            assert_eq!(
                (s1, s2),
                weak_init(&data[p..p + n]),
                "shrink state diverges at position {p} (k={n})"
            );
            s1 = s1.wrapping_sub(u32::from(data[p]));
            #[expect(clippy::cast_possible_truncation)]
            let k = n as u32;
            s2 = s2.wrapping_sub(k.wrapping_mul(u32::from(data[p])));
            if p + n < data.len() {
                let inn = data[p + n];
                s1 = s1.wrapping_add(u32::from(inn));
                s2 = s2.wrapping_add(s1);
            } else {
                n -= 1; // window shrinks at EOF
            }
        }
    }

    #[test]
    fn block_size_matches_rsync() {
        // The values measured against rsync 3.4.1 over ssh.
        assert_eq!(block_size(33_554_432), 5_792); // 32 MiB
        assert_eq!(block_size(268_435_456), 16_384); // 256 MiB
        assert_eq!(block_size(536_870_912), 23_168); // 512 MiB
        assert_eq!(block_size(1_073_741_824), 32_768); // 1 GiB
        // Clamps.
        assert_eq!(block_size(100_000), 700);
        assert_eq!(block_size(u64::MAX), 32 * 1024);
    }

    #[test]
    fn value_changes_when_content_changes() {
        let a = b"hello world, this is a window of data!";
        let b = b"hello world, this is a wlndow of data!";
        assert_eq!(a.len(), b.len());
        assert_ne!(weak_value(weak_init(a).0, weak_init(a).1), weak_value(weak_init(b).0, weak_init(b).1));
    }
}
