//! Atomic staged-file sink for the sync engine.
//!
//! A [`StagedFile`] is a uniquely-named temporary file in the same directory
//! as the destination, preallocated, written in-place, then atomically
//! renamed over the destination on commit.
//!
//! The receiver applies a delta into the staged file (copy ops reference the
//! destination basis opened separately; literal ops carry the changed bytes),
//! so the destination is only ever replaced atomically — a crash or error
//! leaves only a `.tmp` behind, never a half-written destination.
//!
//! Adapted from pxs `tools/staging.rs`.

use super::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Files at or below this size skip the `posix_fallocate` preallocation
/// (the write pass grows them anyway) — the small-file batch path (files up
/// to 2 MiB, per `SMALL_FILE_MAX`), which can hold thousands of files,
/// otherwise pays one allocation syscall per file.
const SMALL_PREALLOC_MAX: u64 = 2 * 1024 * 1024;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static STAGED_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Temporary file used to stage an atomic replacement of a destination path.
#[derive(Debug)]
pub struct StagedFile {
    final_path: PathBuf,
    staged_path: PathBuf,
    committed: AtomicBool,
}

impl StagedFile {
    /// Create a new staging file descriptor for `final_path`.
    ///
    /// The temporary file is placed in the same directory so the final rename
    /// is atomic on the same filesystem.
    ///
    /// # Errors
    ///
    /// Returns an error if the destination has no parent directory or file name.
    pub fn new(final_path: &Path) -> io::Result<Self> {
        let staged_path = unique_sibling_path(final_path, None)?;
        Ok(Self {
            final_path: final_path.to_path_buf(),
            staged_path,
            committed: AtomicBool::new(false),
        })
    }

    /// Path of the staging file on disk.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.staged_path
    }

    /// Create and initialize the staging file.
    ///
    /// Ensures the parent directory exists, creates the staging file
    /// exclusively, and preallocates to `size` bytes — unless `sparse` is
    /// set (`-S`), when `set_len` is used instead: the posix_fallocate-style
    /// preallocation would allocate real blocks and eat the holes the sparse
    /// writer creates. `set_len` extends with a hole, which is exactly what
    /// the sparse write path needs.
    ///
    /// # Errors
    ///
    /// Returns an error if the staging file cannot be created or initialized.
    pub fn prepare(&self, size: u64, sparse: bool) -> io::Result<()> {
        if let Some(parent) = self.staged_path.parent() {
            make_dir_chain(parent)?;
        }

        let staged = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&self.staged_path)?;
        let result = if sparse {
            // Hole-extending truncate instead of block-allocating preallocation
            // (see the doc comment above).
            staged.set_len(size)
        } else if size <= SMALL_PREALLOC_MAX {
            // Small files (the batch path, ≤ 2 MiB): the write pass grows
            // the file — the preallocation's block-allocation cost is pure
            // overhead (the small-file batch can hold thousands of files).
            Ok(())
        } else {
            fs::preallocate(&staged, size)
        };
        drop(staged);
        result
    }

    /// Open the staging file for reading and writing.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened.
    pub fn open(&self) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.staged_path)
    }

    /// Atomically replace the destination with the staging file.
    ///
    /// Safe to call from concurrent tasks; only the first caller performs the
    /// rename. When `fsync` is set the file is synced before the rename
    /// (portable `sync_all`); the default is no per-file fsync — see the
    /// sync tool's `--fsync` flag. When `backup` is set, an existing regular
    /// file at the destination is first renamed to `<name>~` (rsync
    /// `--backup`), replacing any older backup.
    ///
    /// # Errors
    ///
    /// Returns an error if the final path cannot be replaced.
    pub fn commit(&self, fsync: bool, backup: bool) -> io::Result<()> {
        if self.committed.load(Ordering::SeqCst) {
            return Ok(());
        }
        if fsync {
            // Best-effort flush of any still-open handles before the rename.
            if let Ok(file) = self.open() {
                file.sync_all()?;
            }
        }
        install_prepared_path(&self.staged_path, &self.final_path, backup)?;
        self.committed.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Remove the staging file if it still exists.
    ///
    /// # Errors
    ///
    /// Returns an error for reasons other than the file already being absent.
    pub fn cleanup(&self) -> io::Result<()> {
        match std::fs::remove_file(&self.staged_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if self.committed.load(Ordering::SeqCst) {
            return;
        }
        let _ = self.cleanup();
    }
}

/// Ensure every component of `dir` is (or can become) a directory, removing
/// any regular file (or symlink) that blocks the path — the "a file was
/// replaced by a directory" case. The deepest existing ancestor's chain is
/// left untouched; the missing suffix is created.
///
/// # Errors
///
/// Returns an I/O error if a blocking entry cannot be removed or the chain
/// cannot be created.
pub(crate) fn make_dir_chain(dir: &Path) -> io::Result<()> {
    // The common case — a file landing in an existing tree: `StagedFile`
    // calls this for every file, and the `create_dir_all` below would still
    // issue an mkdir(2) that fails EEXIST. Short-circuit on the stat.
    if std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir()) {
        return Ok(());
    }
    let mut current = dir;
    loop {
        match std::fs::symlink_metadata(current) {
            Ok(meta) if meta.is_dir() => break,
            Ok(_) => {
                // A file or symlink occupies this path: remove it so the
                // directory chain can be created underneath.
                super::fs::remove_file_any(current)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => match current.parent() {
                Some(parent) => current = parent,
                None => break,
            },
            Err(error) => return Err(error),
        }
    }
    std::fs::create_dir_all(dir)
}

/// Atomically install `prepared_path` at `final_path`.
///
/// If `final_path` names a directory, it is first moved aside and only removed
/// after the prepared file has been installed; on failure the original is
/// restored. With `backup` set, an existing regular file is first renamed to
/// `<name>~` (rsync `--backup`).
///
/// # Errors
///
/// Returns an error if the replacement cannot be installed.
fn install_prepared_path(prepared_path: &Path, final_path: &Path, backup: bool) -> io::Result<()> {
    let mut replaced = match std::fs::symlink_metadata(final_path) {
        Ok(meta) if meta.is_dir() => Some(move_aside(final_path)?),
        Ok(_) => {
            // Windows: `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` cannot
            // replace a file with the read-only attribute set (Unix `rename`
            // is not so picky), so clear it first — the file is being
            // replaced anyway.
            #[cfg(windows)]
            clear_readonly_attribute(final_path)?;
            if backup {
                backup_existing(final_path)?;
            }
            None
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    match std::fs::rename(prepared_path, final_path) {
        Ok(()) => {
            if let Some((original, backup)) = replaced.take() {
                // A directory replaced by a file loses its contents: warn so
                // the removal is visible (the source is authoritative).
                let entry_count = std::fs::read_dir(&backup).map_or(0, std::iter::Iterator::count);
                if entry_count > 0 {
                    tracing::warn!(
                        "replacing non-empty directory {original:?} with a file — {} entry(ies) removed",
                        entry_count
                    );
                }
                fs::remove_path_if_exists(&backup)?;
            }
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_path_if_exists(prepared_path);
            if let Some((original, backup)) = replaced {
                let _ = restore_moved(&original, &backup);
            }
            Err(error)
        }
    }
}

/// Clear the read-only attribute on `path` so the file can be replaced by a
/// rename (Windows only; see `install_prepared_path`).
#[cfg(windows)]
fn clear_readonly_attribute(path: &Path) -> io::Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Move a path to a unique sibling backup, returning (original, backup).
fn move_aside(path: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let backup = unique_sibling_path(path, Some("backup"))?;
    std::fs::rename(path, &backup)?;
    Ok((path.to_path_buf(), backup))
}

/// Move the existing regular file at `path` to `<path>~` (rsync `--backup`),
/// removing any older backup first. Directories are not backed up (the caller
/// only reaches here for non-directory entries).
///
/// `pub(crate)` so the receiver can also back up files about to be *deleted*
/// (rsync backs those up too), not only files about to be replaced.
///
/// # Errors
///
/// Returns an I/O error if the old backup cannot be removed or the rename
/// fails.
pub(crate) fn backup_existing(path: &Path) -> io::Result<()> {
    let mut backup = path.as_os_str().to_os_string();
    backup.push("~");
    let backup = PathBuf::from(backup);
    match std::fs::symlink_metadata(&backup) {
        Ok(_) => std::fs::remove_file(&backup)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::rename(path, &backup)
}

/// Restore a moved-aside backup to its original location.
fn restore_moved(original: &Path, backup: &Path) -> io::Result<()> {
    let _ = fs::remove_path_if_exists(original);
    std::fs::rename(backup, original)
}

/// Produce a unique sibling path for `path`, with an optional `tag` in the
/// name (the staging sink uses no tag; move-aside backups use "backup").
fn unique_sibling_path(path: &Path, tag: Option<&str>) -> io::Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let file_name = file_name.to_string_lossy();

    loop {
        let counter = STAGED_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = match tag {
            Some(tag) => {
                format!(".{file_name}.cp2.{tag}.{pid}.{counter}.tmp", pid = std::process::id())
            }
            None => format!(".{file_name}.cp2.{pid}.{counter}.tmp", pid = std::process::id()),
        };
        let candidate = parent.join(name);
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_commit_roundtrip() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.bin");
        let staged = StagedFile::new(&final_path).unwrap();
        staged.prepare(8, false).unwrap();
        {
            let mut file = staged.open().unwrap();
            file.write_all(b"abcdefgh").unwrap();
        }
        staged.commit(false, false).unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"abcdefgh");
    }

    #[test]
    fn staged_commit_backup_keeps_original() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.bin");
        std::fs::write(&final_path, b"old").unwrap();
        let staged = StagedFile::new(&final_path).unwrap();
        staged.prepare(8, false).unwrap();
        {
            let mut file = staged.open().unwrap();
            file.write_all(b"abcdefgh").unwrap();
        }
        staged.commit(false, true).unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"abcdefgh");
        assert_eq!(std::fs::read(dir.path().join("out.bin~")).unwrap(), b"old");
    }

    #[test]
    fn staged_commit_no_backup_replaces() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.bin");
        std::fs::write(&final_path, b"old").unwrap();
        let staged = StagedFile::new(&final_path).unwrap();
        staged.prepare(3, false).unwrap();
        {
            let mut file = staged.open().unwrap();
            file.write_all(b"new").unwrap();
        }
        staged.commit(false, false).unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"new");
        // No backup left behind.
        assert!(!dir.path().join("out.bin~").exists());
    }

    #[test]
    fn staged_commit_backup_overwrites_stale_backup() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.bin");
        std::fs::write(&final_path, b"old").unwrap();
        std::fs::write(dir.path().join("out.bin~"), b"stale").unwrap();
        let staged = StagedFile::new(&final_path).unwrap();
        staged.prepare(3, false).unwrap();
        {
            let mut file = staged.open().unwrap();
            file.write_all(b"new").unwrap();
        }
        staged.commit(false, true).unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"new");
        assert_eq!(std::fs::read(dir.path().join("out.bin~")).unwrap(), b"old");
    }

    #[test]
    fn staged_drop_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.bin");
        let staged = StagedFile::new(&final_path).unwrap();
        staged.prepare(16, false).unwrap();
        let tmp = staged.path().to_path_buf();
        assert!(tmp.exists());
        drop(staged);
        assert!(!tmp.exists());
    }

    #[test]
    fn staged_rejects_no_parent() {
        assert!(StagedFile::new(Path::new("/")).is_err());
    }
}
