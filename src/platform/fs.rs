//! Platform filesystem primitives for the sync engine.
//!
//! Provides the low-level operations the receiver's atomic apply needs:
//!
//! - **Delta basis** ([`open_basis`]): open the current destination as the
//!   delta basis, or `None` when it is missing or a directory.
//! - **Preallocation** ([`preallocate`]): avoid ENOSPC mid-transfer and file
//!   fragmentation.
//! - **Metadata** ([`FileMeta`], [`apply_meta`]): mode + mtime to ns
//!   precision, applied to the destination after transfer. **Owner/group are
//!   never touched** — cp2's 0-Root rule: every destination file belongs to
//!   the SSH connection user, so there is no `chown` anywhere.
//! - **Durability** ([`sync_dir`]): make completed renames durable.
//!
//! Adapted from pxs `tools/staging.rs` + `net/protocol.rs`.

#[cfg(unix)]
use std::ffi::{c_char, c_void, CStr};
use std::fs::File;
use std::io;
use std::path::Path;

/// Portable file metadata captured at scan time and applied after transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    /// Size in bytes.
    pub size: u64,
    /// POSIX permission bits (Unix). On Windows the scanner emits a fixed
    /// `0644` placeholder — mode is never applied there.
    pub mode: u32,
    /// Modification time, seconds since UNIX epoch.
    pub mtime_sec: i64,
    /// Modification time, nanosecond remainder.
    pub mtime_nsec: u32,
    /// Last-access time, seconds since UNIX epoch — restored only under
    /// `--atimes` (otherwise `UTIME_OMIT` keeps the receiver's atime).
    pub atime_sec: i64,
    /// Last-access time, nanosecond remainder.
    pub atime_nsec: u32,
}

/// Open `path` as a delta basis: `Ok(None)` when the path is missing or is a
/// directory (a directory cannot be a basis — the incoming file replaces it
/// atomically at commit). Platform-independent: `File::open` on a directory
/// succeeds on Unix but fails with `PermissionDenied` on Windows.
///
/// # Errors
///
/// Returns an I/O error for non-missing, non-directory open failures.
pub fn open_basis(path: &Path) -> io::Result<Option<File>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(None),
        Ok(_) => Ok(Some(File::open(path)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Remove a file, working around the Windows read-only attribute (which
/// blocks deletion) by clearing it first and retrying.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be removed.
pub fn remove_file_any(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            #[cfg(windows)]
            {
                // The read-only attribute blocks deletion on Windows.
                if let Ok(meta) = std::fs::metadata(path) {
                    let mut perms = meta.permissions();
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    let _ = std::fs::set_permissions(path, perms);
                }
                std::fs::remove_file(path)
            }
            #[cfg(not(windows))]
            {
                // On Unix a read-only file is still removable (directory
                // permissions govern), so this is a genuine failure.
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Remove whatever occupies `path` (file, symlink, or directory), tolerating
/// absence. Files go through [`remove_file_any`] (Windows read-only aware);
/// directories are removed recursively. Shared by the staging sink and the
/// receiver.
///
/// # Errors
///
/// Returns an I/O error for reasons other than the path already being absent.
pub(crate) fn remove_path_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => remove_file_any(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// The file's mtime in whole seconds since the Unix epoch (the manifest's
/// granularity), clamping pre-epoch times to 0. Canonical conversion shared by
/// the scanner (manifest), sender (source re-stat), and receiver (destination
/// re-stat).
#[must_use]
pub(crate) fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The file's mtime nanosecond remainder (see [`mtime_secs`]) — carried so
/// `-a` (archive) can restore sub-second fidelity.
#[must_use]
pub(crate) fn mtime_nsecs(meta: &std::fs::Metadata) -> u32 {
    meta.modified()
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos())
}

/// The file's atime in whole seconds since the Unix epoch — always captured
/// at scan time, restored on the receiver only under `--atimes`.
#[must_use]
pub(crate) fn atime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.accessed()
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The file's atime nanosecond remainder (see [`atime_secs`]) — carried so
/// `-U`/`--atimes` can restore sub-second fidelity.
#[must_use]
pub(crate) fn atime_nsecs(meta: &std::fs::Metadata) -> u32 {
    meta.accessed()
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos())
}

/// Restore a file's owner/group (`-a` archive mode, best-effort — 0-Root
/// stays the default: the owner is the SSH connection user, no chown unless
/// asked). A non-root receiver cannot chown to another uid/gid — the attempt
/// fails with EPERM and the caller warns, keeping the SSH user's ownership.
/// Entries already owned by the receiver's own uid/gid are skipped. Symlinks
/// use `lchown` (the link itself is never followed).
///
/// # Errors
///
/// Returns an I/O error when the chown fails (EPERM as a non-root receiver,
/// EPERM/EACCES on a read-only filesystem, ...).
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn apply_owner(
    path: &Path,
    uid: Option<u32>,
    gid: Option<u32>,
    is_symlink: bool,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let (Some(uid), Some(gid)) = (uid, gid) else {
            return Ok(());
        };
        // Already ours (the common single-user case): nothing to do.
        // SAFETY: geteuid/getegid take no arguments.
        if uid == unsafe { libc::geteuid() } && gid == unsafe { libc::getegid() } {
            return Ok(());
        }
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))?;
        // SAFETY: `cpath` is a valid NUL-terminated path; uid/gid are in range.
        let rc = unsafe {
            if is_symlink {
                libc::lchown(cpath.as_ptr(), uid, gid)
            } else {
                libc::chown(cpath.as_ptr(), uid, gid)
            }
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, uid, gid, is_symlink);
        Ok(())
    }
}

/// Preallocate `file` to at least `size` bytes.
///
/// On Linux this uses `posix_fallocate`; macOS, Windows, and other platforms
/// fall back to `set_len`, which extends the file to `size` bytes. Helps avoid
/// ENOSPC mid-transfer and filesystem fragmentation.
///
/// # Errors
///
/// Returns an I/O error on failure.
#[cfg(target_os = "linux")]
pub fn preallocate(file: &File, size: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // Sizes beyond i64::MAX cannot be expressed as an `off_t`.
    let off_size = i64::try_from(size).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "file size exceeds supported range",
        )
    })?;
    // SAFETY: fd is a valid open file; arguments are in range.
    // posix_fallocate returns 0 on success or an errno-style code, not -1.
    let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, off_size) };
    if rc == 0 {
        return Ok(());
    }
    // EINTR/EOPNOTSUPP/EINVAL/ENOSYS mean "not supported here" — fall back
    // to a plain length set. Anything else is a real error (e.g. ENOSPC).
    if matches!(
        rc,
        libc::EINTR | libc::EOPNOTSUPP | libc::EINVAL | libc::ENOSYS
    ) {
        file.set_len(size)
    } else {
        Err(io::Error::from_raw_os_error(rc))
    }
}

/// macOS's xattr API appends arguments Linux/BSD lack: `listxattr` takes an
/// `options` flag and `getxattr`/`setxattr` take `position` + `flags` (all
/// zero for plain path-relative access). The helpers below pair the shapes.
#[cfg(all(unix, not(target_os = "macos")))]
fn xattr_list_size(path: &CStr) -> isize {
    // SAFETY: `path` is a valid NUL-terminated string; `null` is the
    // size-probe form.
    unsafe { libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0) }
}
#[cfg(target_os = "macos")]
fn xattr_list_size(path: &CStr) -> isize {
    // SAFETY: `path` is a valid NUL-terminated string; `null` is the
    // size-probe form.
    unsafe { libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0, 0) }
}
#[cfg(all(unix, not(target_os = "macos")))]
fn xattr_list(path: &CStr, list: *mut c_char, len: usize) -> isize {
    // SAFETY: `path` is a valid NUL-terminated string; `list` is writable
    // for `len` bytes.
    unsafe { libc::listxattr(path.as_ptr(), list, len) }
}
#[cfg(target_os = "macos")]
fn xattr_list(path: &CStr, list: *mut c_char, len: usize) -> isize {
    // SAFETY: `path` is a valid NUL-terminated string; `list` is writable
    // for `len` bytes.
    unsafe { libc::listxattr(path.as_ptr(), list, len, 0) }
}
#[cfg(all(unix, not(target_os = "macos")))]
fn xattr_get_size(path: &CStr, name: &CStr) -> isize {
    // SAFETY: `path` and `name` are valid NUL-terminated strings; `null`
    // is the size-probe form.
    unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) }
}
#[cfg(target_os = "macos")]
fn xattr_get_size(path: &CStr, name: &CStr) -> isize {
    // SAFETY: `path` and `name` are valid NUL-terminated strings; `null`
    // is the size-probe form.
    unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) }
}
#[cfg(all(unix, not(target_os = "macos")))]
fn xattr_get(path: &CStr, name: &CStr, value: *mut c_void, len: usize) -> isize {
    // SAFETY: `path` and `name` are valid NUL-terminated strings; `value`
    // is writable for `len` bytes.
    unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), value, len) }
}
#[cfg(target_os = "macos")]
fn xattr_get(path: &CStr, name: &CStr, value: *mut c_void, len: usize) -> isize {
    // SAFETY: `path` and `name` are valid NUL-terminated strings; `value`
    // is writable for `len` bytes.
    unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), value, len, 0, 0) }
}
#[cfg(all(unix, not(target_os = "macos")))]
fn xattr_set(path: &CStr, name: &CStr, value: *const c_void, len: usize) -> libc::c_int {
    // SAFETY: `path` and `name` are valid NUL-terminated strings; `value`
    // is readable for `len` bytes.
    unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), value, len, 0) }
}
#[cfg(target_os = "macos")]
fn xattr_set(path: &CStr, name: &CStr, value: *const c_void, len: usize) -> libc::c_int {
    // SAFETY: `path` and `name` are valid NUL-terminated strings; `value`
    // is readable for `len` bytes.
    unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), value, len, 0, 0) }
}

/// Collect the path's extended attributes (`--xattrs`): name/value pairs for
/// every readable attribute. Best-effort — an unreadable attribute (or an
/// attribute list that cannot be read at all, e.g. on a filesystem without
/// xattr support) simply contributes nothing. A no-op on Windows, which has
/// no POSIX xattr API.
#[must_use]
pub fn collect_xattrs(path: &Path) -> Vec<(String, Vec<u8>)> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
            return Vec::new();
        };
        let size = xattr_list_size(&cpath);
        if size <= 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; usize::try_from(size).unwrap_or(0)];
        let n = xattr_list(&cpath, buf.as_mut_ptr().cast(), buf.len());
        if n <= 0 {
            return Vec::new();
        }
        buf.truncate(usize::try_from(n).unwrap_or(0));
        let mut out = Vec::new();
        for name in buf.split(|&b| b == 0).filter(|s| !s.is_empty()) {
            // A Rust `String` is not guaranteed NUL-terminated; the xattr
            // name must be a real C string or the kernel reads past it.
            let Ok(name) = std::ffi::CString::new(name) else {
                continue;
            };
            let vsize = xattr_get_size(&cpath, &name);
            if vsize <= 0 {
                continue;
            }
            let mut value = vec![0u8; usize::try_from(vsize).unwrap_or(0)];
            // A concurrent remove may shrink the value (ERANGE) — skip.
            let vn = xattr_get(&cpath, &name, value.as_mut_ptr().cast(), value.len());
            if vn <= 0 {
                continue;
            }
            value.truncate(usize::try_from(vn).unwrap_or(0));
            out.push((name.to_string_lossy().into_owned(), value));
        }
        out
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Vec::new()
    }
}

/// Apply extended attributes to `path` (`--xattrs`): `setxattr` per name.
/// Best-effort at the caller's discretion — a name that cannot be set (a
/// `security.*` attribute as a non-root receiver, a read-only filesystem, ...)
/// is skipped and remembered as the first error, so one warning covers the
/// file. A no-op on Windows.
///
/// # Errors
///
/// Returns the first per-name error after attempting every attribute.
pub fn apply_xattrs(path: &Path, xattrs: &[(String, Vec<u8>)]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"));
        };
        let mut first_err: Option<io::Error> = None;
        for (name, value) in xattrs {
            // A Rust `String` is not guaranteed NUL-terminated; the xattr
            // name must be a real C string.
            let Ok(cname) = std::ffi::CString::new(name.as_str()) else {
                continue;
            };
            let rc = xattr_set(&cpath, &cname, value.as_ptr().cast(), value.len());
            if rc != 0 && first_err.is_none() {
                first_err = Some(io::Error::last_os_error());
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, xattrs);
        Ok(())
    }
}

/// Preallocate `file` to at least `size` bytes.
///
/// # Errors
///
/// Returns an I/O error on failure.
#[cfg(not(target_os = "linux"))]
pub fn preallocate(file: &File, size: u64) -> io::Result<()> {
    file.set_len(size)
}

/// Apply captured metadata to `path`.
///
/// Sets mode and mtime to ns precision. `is_symlink` controls whether the
/// target or the link is updated (modes are never applied to symlinks).
/// `preserve_atime` (`--atimes`) restores the captured last-access time;
/// otherwise atime is left alone (`UTIME_OMIT`). Owner/group are
/// deliberately never applied here (0-Root: every file belongs to the SSH
/// connection user).
///
/// # Errors
///
/// Returns an I/O error if metadata cannot be applied.
#[expect(clippy::fn_params_excessive_bools, reason = "the flags mirror the CLI one-to-one")]
pub fn apply_meta(
    path: &Path,
    meta: &FileMeta,
    is_symlink: bool,
    preserve_mode: bool,
    preserve_times: bool,
    preserve_atime: bool,
) -> io::Result<()> {
    if preserve_mode {
        apply_mode(path, meta.mode, is_symlink)?;
    }
    if preserve_times || preserve_atime {
        apply_times(path, meta, preserve_times, preserve_atime, is_symlink)?;
    }
    Ok(())
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: u32, is_symlink: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if is_symlink {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn apply_mode(_path: &Path, _mode: u32, _is_symlink: bool) -> io::Result<()> {
    Ok(())
}

/// Create a special filesystem object (fifo, socket, or block/char device).
/// Unix-only: `mkfifo` needs no privileges; `mknod` (sockets and devices)
/// needs root, and an `EPERM` is surfaced for the caller to skip the entry.
/// A no-op elsewhere — Windows has no such objects.
///
/// # Panics
///
/// Panics if `mode`'s permission bits do not fit the platform's `mode_t`
/// (u16 on Apple/FreeBSD) — impossible, because they are masked to `0o777`.
///
/// # Errors
///
/// Returns an I/O error if the object cannot be created (a non-root
/// `mknod` surfaces `EPERM`; `mkfifo` may hit `EEXIST`).
pub fn create_special(
    path: &Path,
    kind: crate::protocol::FileKind,
    rdev: Option<u64>,
    mode: u32,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))?;
        // SAFETY: `cpath` is a valid NUL-terminated path for the duration of the call.
        let rc = unsafe {
            if kind == crate::protocol::FileKind::Fifo {
                libc::mkfifo(
                    cpath.as_ptr(),
                    libc::mode_t::try_from(mode & 0o777).expect("mode fits mode_t"),
                )
            } else {
                let type_bit = match kind {
                    crate::protocol::FileKind::Socket => libc::S_IFSOCK,
                    crate::protocol::FileKind::BlockDevice => libc::S_IFBLK,
                    crate::protocol::FileKind::CharDevice => libc::S_IFCHR,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "not a special file kind",
                        ))
                    }
                };
                libc::mknod(
                    cpath.as_ptr(),
                    type_bit | libc::mode_t::try_from(mode & 0o777).expect("mode fits mode_t"),
                    rdev.unwrap_or(0) as libc::dev_t,
                )
            }
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, kind, rdev, mode);
        Ok(())
    }
}

#[cfg(unix)]
fn apply_times(
    path: &Path,
    meta: &FileMeta,
    preserve_times: bool,
    preserve_atime: bool,
    is_symlink: bool,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))?;
    let times = [
        if preserve_atime {
            libc::timespec {
                tv_sec: meta.atime_sec,
                tv_nsec: meta.atime_nsec.into(),
            }
        } else {
            // `UTIME_OMIT` leaves the receiver's atime untouched (the write
            // just set it); only `--atimes` restores the source's.
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            }
        },
        if preserve_times {
            libc::timespec {
                tv_sec: meta.mtime_sec,
                tv_nsec: meta.mtime_nsec.into(),
            }
        } else {
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            }
        },
    ];
    // SAFETY: `cpath` is valid; `times` points to two valid timespecs.
    let rc = unsafe {
        if is_symlink {
            libc::utimensat(
                libc::AT_FDCWD,
                cpath.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } else {
            libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0)
        }
    };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if matches!(err.raw_os_error(), Some(libc::EINTR)) {
        // Rare; retry once.
        // SAFETY: same preconditions as the call above — `cpath` is a valid
        // NUL-terminated path and `times` points to two valid timespecs.
        let rc = unsafe {
            if is_symlink {
                libc::utimensat(
                    libc::AT_FDCWD,
                    cpath.as_ptr(),
                    times.as_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } else {
                libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0)
            }
        };
        if rc == 0 {
            return Ok(());
        }
    }
    Err(err)
}

#[cfg(not(unix))]
fn apply_times(
    path: &Path,
    meta: &FileMeta,
    preserve_times: bool,
    preserve_atime: bool,
    _is_symlink: bool,
) -> io::Result<()> {
    use std::fs::{FileTimes, OpenOptions};
    use std::time::{Duration, UNIX_EPOCH};

    let mut times = FileTimes::new();
    if preserve_times {
        let modified = UNIX_EPOCH
            .checked_add(Duration::new(u64::try_from(meta.mtime_sec).unwrap_or(0), meta.mtime_nsec))
            .unwrap_or(UNIX_EPOCH);
        times = times.set_modified(modified);
    }
    if preserve_atime {
        let accessed = UNIX_EPOCH
            .checked_add(Duration::new(u64::try_from(meta.atime_sec).unwrap_or(0), meta.atime_nsec))
            .unwrap_or(UNIX_EPOCH);
        times = times.set_accessed(accessed);
    }

    // Windows cannot open a directory with a plain `OpenOptions`; the
    // `FILE_FLAG_BACKUP_SEMANTICS` flag allows it (the same trick the
    // `filetime` crate uses), so directory mtimes are preserved on all
    // platforms instead of only Unix.
    #[cfg(windows)]
    if std::fs::symlink_metadata(path)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let dir = OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        return dir.set_times(times);
    }

    let file = OpenOptions::new().write(true).open(path)?;
    file.set_times(times)
}

/// Flush a directory to make completed renames durable (best effort).
///
/// Linux fsyncs an open directory fd; Windows flushes a directory handle
/// opened with `FILE_FLAG_BACKUP_SEMANTICS` (the one way to open a directory
/// there — plain `File::open` fails). Other platforms (macOS: std `File::open`
/// on a directory returns `EISDIR`) are a no-op.
///
/// # Errors
///
/// Returns an I/O error when the directory cannot be opened or flushed; a
/// no-op platform always returns `Ok(())`.
pub fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        std::fs::File::open(path)?.sync_all()
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let dir = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        dir.sync_all()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = path;
        Ok(())
    }
}

/// Enlarge a pipe to 1 MiB (best-effort, Unix only): the default 64 KiB
/// capacity forces a writer/reader wakeup round trip every 64 KiB, which
/// dominates per-frame latency where wakeups are expensive (e.g. WSL2) and
/// limits throughput on fast links. `pipe-max-size` caps the value; a smaller
/// system limit silently keeps the default, and a failure is ignored (the
/// pipe still works, just at the smaller capacity).
///
/// On Windows the pipe buffer is fixed at creation time and cannot be
/// resized from the child side — a no-op (the anonymous pipes' default
/// buffering is functional, just smaller).
#[cfg(unix)]
pub fn enlarge_pipe<F: std::os::fd::AsRawFd>(fd: &F) {
    // F_SETPIPE_SZ (Linux): 1031. Non-Linux Unixes ignore the unknown
    // command (best-effort).
    const F_SETPIPE_SZ: libc::c_int = 1031;
    let _ = unsafe { libc::fcntl(fd.as_raw_fd(), F_SETPIPE_SZ, 1024 * 1024) };
}

/// Windows (and other non-Unix) fallback: no-op.
#[cfg(not(unix))]
pub fn enlarge_pipe<F>(fd: &F) {
    let _ = fd;
}

/// The user cache directory for cp2's signature cache: `~/.cache/cp2/
/// sig-cache` on Unix, `%LOCALAPPDATA%\cp2\sig-cache` on Windows. `None`
/// when no home is known — the cache is then disabled and basis signing
/// falls back to reading the file every run.
#[must_use]
pub(crate) fn sig_cache_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(|h| std::path::PathBuf::from(h).join("AppData").join("Local"))
            })
            .map(|base| base.join("cp2").join("sig-cache"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(|h| {
            std::path::PathBuf::from(h)
                .join(".cache")
                .join("cp2")
                .join("sig-cache")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prealloc_sets_len() {
        use std::fs::OpenOptions;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.bin");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        preallocate(&file, 1 << 20).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 1 << 20);
    }

    #[cfg(unix)]
    #[test]
    fn apply_owner_skips_self_and_fails_foreign() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        let uid = meta.uid();
        let gid = meta.gid();
        // Same owner as the process: skipped, Ok.
        assert!(apply_owner(&file, Some(uid), Some(gid), false).is_ok());
        // SAFETY: geteuid takes no arguments.
        if unsafe { libc::geteuid() } == 0 {
            return; // root can chown anywhere — nothing more to assert
        }
        // A foreign owner: EPERM as a non-root receiver.
        let err = apply_owner(&file, Some(uid.wrapping_add(1)), Some(gid), false).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn open_basis_treats_dir_and_missing_as_none() {
        let dir = tempfile::tempdir().unwrap();
        // Missing path: no basis.
        assert!(open_basis(&dir.path().join("nope")).unwrap().is_none());
        // A directory is not a basis (and must not fail to open on Windows).
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(open_basis(&sub).unwrap().is_none());
        // A regular file is a basis.
        let file = dir.path().join("f.bin");
        std::fs::write(&file, b"data").unwrap();
        let basis = open_basis(&file).unwrap();
        assert!(basis.is_some());
        drop(basis);
    }
}


