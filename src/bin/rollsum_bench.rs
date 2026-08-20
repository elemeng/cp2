//! Rollsum engine microbenchmark: per-phase throughput of the rsync-style
//! delta on a pseudo-random file — signature generation, the byte-scan
//! against an unrelated basis (pure per-byte cost), a mid-file 10 MiB edit
//! (the realistic delta case), and the identical file (all-match).
//!
//! Each scenario verifies byte-exact reconstruction before printing.
//!
//! Usage: `cargo run --release --bin rollsum_bench [--mb N]`

use std::io::Cursor;
use std::time::Instant;

use cp2::delta::{Delta, Signature, apply_patch, compute_delta_rollsum};

/// Deterministic pseudo-random data (xorshift64, seeded so the "basis" and
/// "source" streams differ — the classic same-seed pitfall).
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x = seed;
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push((x & 0xFF) as u8);
    }
    v
}

/// MiB as f64 (bench display only) — honest 2^20 divisor.
fn mib(bytes: u64) -> f64 {
    #[expect(clippy::cast_precision_loss)]
    let b = bytes as f64;
    b / (1024.0 * 1024.0)
}

fn gbps(bytes: u64, secs: f64) -> f64 {
    #[expect(clippy::cast_precision_loss)]
    let b = bytes as f64;
    b / secs / 1e9
}

fn verify(basis: &[u8], delta: &Delta, source: &[u8]) {
    let mut out = Vec::new();
    apply_patch(Cursor::new(basis), delta, &mut out, None).unwrap();
    assert_eq!(out, source, "reconstruction mismatch");
}

/// Standalone weak checksum init (mirror of the engine's unrolled loop) so
/// the micro-benchmark needs no crate internals.
fn weak_init_bench(window: &[u8]) -> (u32, u32) {
    let mut s1 = 0u32;
    let mut s2 = 0u32;
    let mut i = 0usize;
    while i + 4 <= window.len() {
        let b0 = u32::from(window[i]);
        let b1 = u32::from(window[i + 1]);
        let b2 = u32::from(window[i + 2]);
        let b3 = u32::from(window[i + 3]);
        s2 = s2.wrapping_add((s1.wrapping_add(b0) << 2).wrapping_add(3 * b1 + 2 * b2 + b3));
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

/// Layered micro-benchmark of the miss path: isolates the per-byte cost of
/// (a) the rolling checksum alone, (b) + the hash-table probe (256 KiB flat
/// head, same shape as the engine), (c) + the literal Vec push.
fn micro_bench(len: usize, block: usize) {
    let data = pseudo_random(len, 0x1357_9BDF);
    let n = len as u64;

    // (a) roll only.
    let (mut s1, mut s2) = weak_init_bench(&data[..block]);
    let mut acc: u64 = 0;
    let t = Instant::now();
    let mut q = 0usize;
    while q + block + 1 < data.len() {
        let out = data[q];
        let inn = data[q + block];
        // Chain-broken form (see the crate's state_roll): both halves use
        // the old values, so the two dependent adds run in parallel.
        s1 = s1.wrapping_sub(u32::from(out));
        #[expect(clippy::cast_possible_truncation)]
        let k = block as u32;
        s2 = s2
            .wrapping_add(s1)
            .wrapping_add(u32::from(inn))
            .wrapping_sub(k.wrapping_add(1).wrapping_mul(u32::from(out)));
        s1 = s1.wrapping_add(u32::from(inn));
        acc = acc.wrapping_add(u64::from(out));
        q += 1;
    }
    let dt = t.elapsed().as_secs_f64();
    println!(
        "micro roll:        {:>7.3}s  {:>6.2} GB/s  (acc={acc})",
        dt,
        gbps(n, dt)
    );
    std::hint::black_box((s1, s2));

    // (b) + flat 256 KiB head probe (all-empty table: pure probe cost).
    let (mut s1, mut s2) = weak_init_bench(&data[..block]);
    #[expect(clippy::large_stack_arrays)]
    let head: Box<[u32; 1 << 16]> = Box::new([u32::MAX; 1 << 16]);
    let mut hits: u64 = 0;
    let t = Instant::now();
    let mut q = 0usize;
    while q + block + 1 < data.len() {
        let out = data[q];
        let inn = data[q + block];
        // Chain-broken form (see the crate's state_roll): both halves use
        // the old values, so the two dependent adds run in parallel.
        s1 = s1.wrapping_sub(u32::from(out));
        #[expect(clippy::cast_possible_truncation)]
        let k = block as u32;
        s2 = s2
            .wrapping_add(s1)
            .wrapping_add(u32::from(inn))
            .wrapping_sub(k.wrapping_add(1).wrapping_mul(u32::from(out)));
        s1 = s1.wrapping_add(u32::from(inn));
        let entry = (s1.wrapping_add(s2) & 0xffff) as usize;
        let first = head[entry];
        hits = hits.wrapping_add(u64::from(first != u32::MAX));
        q += 1;
    }
    let dt = t.elapsed().as_secs_f64();
    println!(
        "micro roll+probe:  {:>7.3}s  {:>6.2} GB/s  (hits={hits})",
        dt,
        gbps(n, dt)
    );
    std::hint::black_box((s1, s2));

    // (c) + literal Vec push.
    let (mut s1, mut s2) = weak_init_bench(&data[..block]);
    let mut literal: Vec<u8> = Vec::new();
    let t = Instant::now();
    let mut q = 0usize;
    while q + block + 1 < data.len() {
        let out = data[q];
        let inn = data[q + block];
        // Chain-broken form (see the crate's state_roll): both halves use
        // the old values, so the two dependent adds run in parallel.
        s1 = s1.wrapping_sub(u32::from(out));
        #[expect(clippy::cast_possible_truncation)]
        let k = block as u32;
        s2 = s2
            .wrapping_add(s1)
            .wrapping_add(u32::from(inn))
            .wrapping_sub(k.wrapping_add(1).wrapping_mul(u32::from(out)));
        s1 = s1.wrapping_add(u32::from(inn));
        literal.push(out);
        q += 1;
    }
    let dt = t.elapsed().as_secs_f64();
    println!(
        "micro roll+push:   {:>7.3}s  {:>6.2} GB/s  (lit={})",
        dt,
        gbps(n, dt),
        literal.len()
    );
    std::hint::black_box((s1, s2));
}

fn main() {
    let micro = std::env::args().any(|a| a == "--micro");
    let mb: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.trim_start_matches("--mb=").parse().ok())
        .unwrap_or(512);
    let len = mb * 1024 * 1024;
    let block = cp2::delta::block_size(len as u64);
    println!("file {mb} MiB ({len} B), block {block} B");

    if micro {
        micro_bench(len, block);
        return;
    }

    let basis = pseudo_random(len, 0xDEAD_BEEF);
    let source = pseudo_random(len, 0xCAFE_F00D);
    let n = len as u64;

    // 1. Signature generation (receiver-side phase).
    let t = Instant::now();
    let sig = Signature::generate_rollsum(&mut Cursor::new(&basis), block).unwrap();
    let dt = t.elapsed().as_secs_f64();
    println!(
        "sig gen:    {:>7.3}s  {:>6.2} GB/s  ({} blocks)",
        dt,
        gbps(n, dt),
        sig.chunks.len()
    );

    // 2. Scan against an unrelated basis — no matches anywhere, so this is
    //    the pure per-byte roll+probe cost.
    let t = Instant::now();
    let delta = compute_delta_rollsum(&mut Cursor::new(&source), &sig, u64::MAX, false).unwrap();
    let dt = t.elapsed().as_secs_f64();
    println!(
        "scan none:  {:>7.3}s  {:>6.2} GB/s  (matched {:.1} MiB, literal {:.1} MiB)",
        dt,
        gbps(n, dt),
        mib(delta.bytes_matched()),
        mib(delta.bytes_literal())
    );
    verify(&basis, &delta, &source);

    // 3. Scan after a mid-file 10 MiB overwrite — the realistic delta case:
    //    one edited region, ≤1-block re-sync at each boundary.
    let mut edited = basis.clone();
    let mid = len / 2;
    for b in &mut edited[mid..mid + 10 * 1024 * 1024] {
        *b ^= 0x5A;
    }
    let t = Instant::now();
    let delta_edit = compute_delta_rollsum(&mut Cursor::new(&edited), &sig, u64::MAX, false).unwrap();
    let dt = t.elapsed().as_secs_f64();
    println!(
        "scan edit:  {:>7.3}s  {:>6.2} GB/s  (matched {:.1} MiB, literal {:.1} MiB)",
        dt,
        gbps(n, dt),
        mib(delta_edit.bytes_matched()),
        mib(delta_edit.bytes_literal())
    );
    verify(&basis, &delta_edit, &edited);

    // 3b. 10 MiB insertion at the middle (shifted tail — the re-sync case).
    let mut inserted = basis.clone();
    inserted.splice(mid..mid, std::iter::repeat_n(0x5A, 10 * 1024 * 1024));
    let t = Instant::now();
    let delta = compute_delta_rollsum(&mut Cursor::new(&inserted), &sig, u64::MAX, false).unwrap();
    let dt = t.elapsed().as_secs_f64();
    println!(
        "scan insrt: {:>7.3}s  (matched {:.1} MiB, literal {:.1} MiB)",
        dt,
        mib(delta.bytes_matched()),
        mib(delta.bytes_literal())
    );
    verify(&basis, &delta, &inserted);

    // 3c. 10 MiB deletion at the middle.
    let mut deleted = basis.clone();
    deleted.drain(mid..mid + 10 * 1024 * 1024);
    let t = Instant::now();
    let delta = compute_delta_rollsum(&mut Cursor::new(&deleted), &sig, u64::MAX, false).unwrap();
    let dt = t.elapsed().as_secs_f64();
    println!(
        "scan delte: {:>7.3}s  (matched {:.1} MiB, literal {:.1} MiB)",
        dt,
        mib(delta.bytes_matched()),
        mib(delta.bytes_literal())
    );
    verify(&basis, &delta, &deleted);

    // 3d. The apply phase: reconstruct the edited source from the basis
    //     (basis read 502 MiB + output write 512 MiB) — the receiver-side
    //     cost rsync also pays.
    let t = Instant::now();
    let mut out = Vec::with_capacity(len);
    {
        let mut basis_r = Cursor::new(&basis);
        apply_patch(&mut basis_r, &delta_edit, &mut out, None).unwrap();
    }
    let dt = t.elapsed().as_secs_f64();
    assert_eq!(out.len(), len);
    println!(
        "apply edit: {:>6.3}s  ({:.1} MiB out, basis read {:.1} MiB)",
        dt,
        mib(len as u64),
        mib(len as u64 - delta_edit.bytes_literal())
    );
    drop(out);

    // 4. Identical file — every window matches at the aligned position.
    let t = Instant::now();
    let delta = compute_delta_rollsum(&mut Cursor::new(&basis), &sig, u64::MAX, false).unwrap();
    let dt = t.elapsed().as_secs_f64();
    println!(
        "scan same:  {:>7.3}s  {:>6.2} GB/s  (matched {:.1} MiB, literal {:.1} MiB)",
        dt,
        gbps(n, dt),
        mib(delta.bytes_matched()),
        mib(delta.bytes_literal())
    );
    verify(&basis, &delta, &basis);
}
