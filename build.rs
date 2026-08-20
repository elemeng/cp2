//! Build fingerprint: a hash of every source file, embedded as
//! `CP2_BUILD_FINGERPRINT`.
//!
//! cp2 has no released v1, so the wire protocol is never locked to a
//! version — the only requirement is that both peers run the *same build*.
//! The auto-deploy and the Hello handshake therefore compare this
//! fingerprint instead of a hand-maintained protocol number: any source
//! change (format, behavior, or performance) automatically forces a
//! redeploy, and a stale remote fails the handshake cleanly instead of
//! misbehaving. A manual `PROTOCOL_VERSION` counter cannot guarantee this —
//! a change that forgets to bump it leaves a stale remote undetected.
//!
//! FNV-1a 64 over the sorted file paths and bytes: deterministic, std-only,
//! and collision-resistant enough for change detection (not security).

use std::fs;

fn main() {
    let mut files: Vec<String> = Vec::new();
    collect_sources("src", &mut files);
    // The fingerprint must be stable across cross-builds of the same tree
    // (e.g. a Windows sidecar compiled on Linux): normalize separators.
    files.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in &files {
        println!("cargo:rerun-if-changed={file}");
        let normalized = file.replace('\\', "/");
        for byte in normalized.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        if let Ok(bytes) = fs::read(file) {
            for byte in bytes {
                // Keep the fingerprint stable across checkouts with different
                // line endings (git autocrlf makes Windows trees CRLF, Unix
                // LF), so both sides of a sync hash the same committed bytes.
                if byte == b'\r' {
                    continue;
                }
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=CP2_BUILD_FINGERPRINT={hash:016x}");
}

fn collect_sources(dir: &str, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sources(&path.to_string_lossy(), out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path.to_string_lossy().into_owned());
        }
    }
}
