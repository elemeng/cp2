//! On-disk cache of basis signatures, keyed by the destination file's
//! (size, mtime) plus a head-and-tail content sample.
//!
//! The sender computes the source's chunk signature as a free byproduct of
//! delta computation (the per-chunk BLAKE3 hashes it needs for matching
//! anyway) and ships it with the transfer; the receiver stores it keyed by
//! the *applied* file's size+mtime. The next run's basis signing
//! (`signature_for_path`) stats the destination and reuses the entry without
//! re-reading the file — content-defined chunking guarantees an unchanged
//! file has an unchanged signature.
//!
//! The (size, mtime) key alone would be one trust step weaker than the
//! quick check's: the quick check's staleness only *skips* a file, while a
//! stale basis signature *writes* — a destination replaced in place with a
//! preserved mtime (restores via `cp -p`, `rsync -t`, `tar --preserve`)
//! would serve a signature of content that is no longer there, and the
//! delta would misapply copy ops against it. Each entry therefore carries a
//! 4 KiB head+tail sample that lookup re-verifies against the live file — a
//! two-ends re-read per basis, negligible next to the signing it saves.
//!
//! Layout: one postcard file per destination path under the cache dir,
//! named by the BLAKE3 hex of the absolute path. No database, no index —
//! the filesystem is the index. Entries are written atomically (temp +
//! rename) and silently ignored when missing, stale, or corrupt.

#![expect(clippy::similar_names)] // mtime_sec / mtime_nsec: canonical pair

use std::path::{Path, PathBuf};

use crate::delta::Signature;

struct SigCache {
    dir: PathBuf,
}

impl SigCache {
    fn new() -> Option<Self> {
        crate::platform::fs::sig_cache_dir().map(|dir| Self { dir })
    }

    #[cfg(test)]
    fn new_at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn entry_path(&self, path: &Path) -> PathBuf {
        let key = blake3::hash(path.to_string_lossy().as_bytes());
        self.dir.join(format!("{}.sig", key.to_hex()))
    }

    fn lookup(
        &self,
        path: &Path,
        file_size: u64,
        mtime_sec: u64,
        mtime_nsec: u32,
    ) -> Option<Signature> {
        let bytes = std::fs::read(self.entry_path(path)).ok()?;
        let (f_size, f_mtime_sec, f_mtime_nsec, sample, signature): (
            u64,
            u64,
            u32,
            [u8; 32],
            Signature,
        ) = postcard::from_bytes(&bytes).ok()?;
        if f_size != file_size || f_mtime_sec != mtime_sec || f_mtime_nsec != mtime_nsec {
            return None;
        }
        // The (size, mtime) key alone would serve a signature of content
        // that was replaced in place with a preserved mtime — the delta
        // would then treat the stale layout as the live basis and write
        // corrupted bytes. Re-verify the head+tail sample; a mismatch is a
        // miss and the stale entry is dropped so the next run re-signs
        // instead of re-checking.
        if content_sample(path) != Some(sample) {
            let _ = std::fs::remove_file(self.entry_path(path));
            return None;
        }
        Some(signature)
    }

    fn store(
        &self,
        path: &Path,
        file_size: u64,
        mtime_sec: u64,
        mtime_nsec: u32,
        signature: &Signature,
    ) {
        // The applied file is on disk at store time (the caller just
        // renamed it into place); a sample that misses here just means no
        // entry — the next run re-signs, which is the safe outcome.
        let Some(sample) = content_sample(path) else {
            return;
        };
        // Serialize the borrowed tuple (size + mtime key, the content
        // sample, then the signature) so a large basis chunk table is not
        // cloned just to be written.
        let Ok(bytes) = postcard::to_allocvec(&(
            file_size,
            mtime_sec,
            mtime_nsec,
            sample,
            signature,
        )) else {
            return;
        };
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let target = self.entry_path(path);
        // Temp + rename: concurrent syncs never observe a torn entry, and a
        // crash mid-write leaves only a stale `.tmp` that is never read.
        let tmp = PathBuf::from(format!("{}.tmp", target.display()));
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &target);
        }
    }
}

/// 4 KiB from each end of the file (the whole file when smaller), hashed
/// together — a cheap content tag that lookup re-verifies so a preserved-
/// mtime replacement of the destination is a cache miss, not a corrupted
/// delta.
fn content_sample(path: &Path) -> Option<[u8; 32]> {
    use std::io::{Read, Seek, SeekFrom};
    const SAMPLE: u64 = 4096;
    // Truncation-safe: the sample is capped at 4096, far below usize::MAX
    // on any 32-bit target.
    #[expect(clippy::cast_possible_truncation)]
    let sample_len = SAMPLE as usize;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; len.min(SAMPLE) as usize];
    file.read_exact(&mut buf).ok()?;
    hasher.update(&buf);
    if len > SAMPLE {
        file.seek(SeekFrom::Start(len - SAMPLE)).ok()?;
        let mut tail = vec![0u8; sample_len];
        file.read_exact(&mut tail).ok()?;
        hasher.update(&tail);
    }
    Some(*hasher.finalize().as_bytes())
}

/// The process-wide cache (created lazily from the user cache directory).
fn cache() -> Option<&'static SigCache> {
    static CACHE: std::sync::OnceLock<Option<SigCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(SigCache::new).as_ref()
}

/// Look up the basis signature of a destination file currently having
/// `size` and `mtime`, if a valid cache entry exists.
pub(crate) fn lookup(
    path: &Path,
    file_size: u64,
    mtime_sec: u64,
    mtime_nsec: u32,
) -> Option<Signature> {
    cache().and_then(|c| c.lookup(path, file_size, mtime_sec, mtime_nsec))
}

/// Store the basis signature of the content `path` will have after the
/// current transfer, keyed by the applied size+mtime.
pub(crate) fn store(
    path: &Path,
    file_size: u64,
    mtime_sec: u64,
    mtime_nsec: u32,
    signature: &Signature,
) {
    if let Some(c) = cache() {
        c.store(path, file_size, mtime_sec, mtime_nsec, signature);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::{ChunkSignature, Signature};
    use std::time::SystemTime;

    fn test_sig(n: u64) -> Signature {
        let chunk = ChunkSignature {
            offset: n,
            len: 100,
            weak: None,
            strong_hash: [(n & 0xFF) as u8; 32],
        };
        Signature {
            file_size: 1000,
            chunks: vec![chunk],
        }
    }

    fn sig_cache(tmp: &Path) -> SigCache {
        SigCache::new_at(tmp.join("sig-cache"))
    }

    #[test]
    fn lookup_hit_and_miss() {
        let tmp = std::env::temp_dir().join(format!("cp2-sigcache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cache = sig_cache(&tmp);
        let path = tmp.join("sub").join("file.bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"hello world").unwrap();

        // Miss on an empty cache.
        assert!(cache.lookup(&path, 11, 1, 2).is_none());

        // Hit with the matching key.
        cache.store(&path, 11, 1, 2, &test_sig(7));
        let got = cache.lookup(&path, 11, 1, 2).expect("hit");
        assert_eq!(got.file_size, 1000);
        assert_eq!(got.chunks[0].offset, 7);

        // Miss when the file changed (any key component differs).
        assert!(cache.lookup(&path, 12, 1, 2).is_none());
        assert!(cache.lookup(&path, 11, 3, 2).is_none());
        assert!(cache.lookup(&path, 11, 1, 9).is_none());

        // A different path has its own entry (path-keyed).
        let other = tmp.join("other.bin");
        std::fs::write(&other, b"x").unwrap();
        assert!(cache.lookup(&other, 1, 1, 2).is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn corrupt_entry_is_a_miss() {
        let tmp = std::env::temp_dir().join(format!("cp2-sigcache-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cache = sig_cache(&tmp);
        let path = tmp.join("file.bin");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&path, b"data").unwrap();

        // Garbage entry: treated as a miss, not an error.
        std::fs::create_dir_all(&cache.dir).unwrap();
        std::fs::write(cache.entry_path(&path), b"not postcard").unwrap();
        assert!(cache.lookup(&path, 4, 1, 2).is_none());

        // Real entry round-trips through the same file.
        cache.store(&path, 4, 1, 2, &test_sig(3));
        assert!(cache.lookup(&path, 4, 1, 2).is_some());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn same_size_same_mtime_replacement_is_a_miss() {
        // The exact staleness that would corrupt a delta: the destination is
        // replaced in place with different content while size and mtime are
        // preserved (cp -p / rsync -t / tar --preserve). The head+tail
        // sample must turn the stale entry into a miss and drop it.
        let tmp = std::env::temp_dir().join(format!("cp2-sigcache-repl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cache = sig_cache(&tmp);
        let path = tmp.join("f.bin");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&path, vec![b'a'; 10 * 4096]).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mtime_sec = crate::platform::fs::mtime_secs(&meta);
        let mtime_nsec = crate::platform::fs::mtime_nsecs(&meta);
        let size = meta.len();
        cache.store(&path, size, mtime_sec, mtime_nsec, &test_sig(1));
        assert!(cache.lookup(&path, size, mtime_sec, mtime_nsec).is_some());

        // Replace the content, preserving size and mtime exactly.
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, vec![b'b'; 10 * 4096]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), size);
        assert_eq!(crate::platform::fs::mtime_secs(&meta), mtime_sec);
        assert_eq!(crate::platform::fs::mtime_nsecs(&meta), mtime_nsec);
        // Same key, different content: the sample catches it, and the stale
        // entry is gone.
        assert!(cache.lookup(&path, size, mtime_sec, mtime_nsec).is_none());
        assert!(!cache.entry_path(&path).exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn signature_is_content_keyed_by_stat() {
        // The key matches what platform::fs::mtime_* report for a real file.
        let tmp = std::env::temp_dir().join(format!("cp2-sigcache-stat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("f.bin");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let (sec, nsec) = (crate::platform::fs::mtime_secs(&meta), crate::platform::fs::mtime_nsecs(&meta));

        let cache = sig_cache(&tmp);
        cache.store(&path, meta.len(), sec, nsec, &test_sig(9));
        let got = cache.lookup(&path, meta.len(), sec, nsec);
        assert_eq!(got.map(|s| s.chunks[0].offset), Some(9));

        // Touching the file changes the mtime -> miss.
        let ft = std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH);
        std::fs::File::options().write(true).open(&path).unwrap().set_times(ft).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert!(cache.lookup(&path, meta.len(), crate::platform::fs::mtime_secs(&meta), crate::platform::fs::mtime_nsecs(&meta)).is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
