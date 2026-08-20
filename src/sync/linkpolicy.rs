//! Pure link/permission decision logic — the "build transfer list" stage.
//!
//! Per the cp2 permissions-and-links spec, every decision about whether
//! content is copied, a link is recreated, or an entry is skipped is made
//! here, at scan time, on the *source* side. The receiver only executes the
//! resulting instructions and never re-derives a decision (spec §0
//! "预决策").
//!
//! The module is pure: no tokio, no filesystem writes, and only the paths
//! callers pass in are inspected (a canonicalize for classification, never a
//! mutation). Everything is unit-testable without a network or an executor.
#![forbid(unsafe_code)]

use crate::protocol::TargetOs;
use std::path::{Component, Path, PathBuf};

/// File extensions that make a Windows-source file executable on a Unix
/// destination (the `exec_hint` heuristic, spec §2.1). Case-insensitive.
const EXEC_HINT_EXTS: &[&str] = &["exe", "bat", "cmd", "ps1", "sh", "pl", "py", "rb", "lua"];

/// The `exec_hint` heuristic (spec §2.1): whether a Windows-source file
/// should carry `+x` when materialized on a Unix destination. Only consulted
/// when the *source* is Windows — a Unix source carries real permission bits.
#[must_use]
pub fn compute_exec_hint(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| EXEC_HINT_EXTS.iter().any(|h| ext.eq_ignore_ascii_case(h)))
}

/// The permission bits a transferred entry carries on the wire, per the
/// spec §2.2 matrix. Computed once on the source side; the receiver applies
/// the value verbatim (on a Windows target, mode application is a no-op, so
/// the value is simply not used there — the NTFS ACL is inherited).
///
/// - `raw_mode`: the source's permission bits (`st_mode & 0o7777`-ish).
/// - `is_dir`: selects the directory/file defaults.
/// - `exec_hint`: [`compute_exec_hint`] on the source path; only consulted
///   for a Windows source.
/// - `no_perms`: `--no-perms`, which overrides everything with explicit
///   defaults (and disables the heuristic).
/// - `keep_special_bits`: `-a` (archive) keeps SUID/SGID/Sticky (`& 0o7777`,
///   byte-identical mode); the default force-clears them (`& 0o777` — the
///   high bits are not meaningful for the SSH user's own files).
#[must_use]
#[expect(clippy::fn_params_excessive_bools)]
pub fn final_mode(
    source_os: TargetOs,
    _target_os: TargetOs,
    raw_mode: u32,
    is_dir: bool,
    exec_hint: bool,
    no_perms: bool,
    keep_special_bits: bool,
) -> u32 {
    if no_perms {
        // Spec §2.2: explicit defaults, never the source's bits.
        return if is_dir { 0o755 } else { 0o644 };
    }
    match source_os {
        TargetOs::Unix => {
            // Keep rwxrwxrwx; `-a` also keeps SUID/SGID/Sticky (byte-identical
            // mode), the default clears them. (The spec's plain `& 0o7777`
            // would retain them unconditionally; the stated intent — clear
            // the high bits by default — requires the archive gate.)
            raw_mode & if keep_special_bits { 0o7777 } else { 0o777 }
        }
        TargetOs::Windows => {
            // Windows has no POSIX bits: 0755 for directories and
            // exec-hinted files, 0644 otherwise.
            if is_dir || exec_hint {
                0o755
            } else {
                0o644
            }
        }
    }
}

/// Compute a relative path from directory `from_dir` to `to`, using `.`/`..`
/// components. Purely lexical (no filesystem access); `from_dir` and `to`
/// must be rooted in the same tree for the result to stay meaningful. The
/// identity case yields `.`.
#[must_use]
pub fn rel_path(from_dir: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component> = from_dir.components().collect();
    let to: Vec<Component> = to.components().collect();
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Rewrite an internal link's literal target to a DEST-relative path: the
/// target's relative path inside the tree, expressed relative to the link's
/// own directory. DEST mirrors the source's relative structure, so the same
/// string resolves correctly at the destination (spec §3.2 — DEST
/// self-containment). Wire paths are '/'-separated.
#[must_use]
pub fn rewrite_internal_target(link_rel: &str, target_rel: &str) -> String {
    let link_dir = Path::new(link_rel)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    rel_path(link_dir, Path::new(target_rel))
        .to_string_lossy()
        .replace('\\', "/")
}

/// How a symlink's target classifies relative to the scan root (spec §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkClass {
    /// Target resolves inside the scan root → recreate as a link with a
    /// rewritten relative target (0 bytes transferred).
    Internal,
    /// Target resolves outside the scan root to a regular file → dereference
    /// and copy the content (default), or keep the absolute target
    /// (`--literal-external-file-links`, Unix targets only).
    ExternalFile,
    /// Target resolves outside the scan root to a directory → skip (default)
    /// or recursed (`--follow-links`).
    ExternalDir,
    /// Target cannot be resolved (dangling) and lexically points outside the
    /// root → skipped like a directory target (there is no content to copy).
    DanglingExternal,
}

/// Classify a symlink at `link_path` (its own lstat path) with literal
/// `target` against the canonical scan `root`. Relative targets resolve
/// against the link's parent; the resolved path is canonicalized when it
/// exists, so chains of links collapse to their real path. A dangling target
/// falls back to a lexical (no-filesystem) containment check.
#[must_use]
pub fn classify_link(link_path: &Path, target: &str, root: &Path) -> LinkClass {
    let resolved = resolve_target(link_path, target);
    let Ok(real) = std::fs::canonicalize(&resolved) else {
        // Dangling: decide lexically whether the target stays inside the
        // root (a dangling internal link is preserved) or escapes it.
        let normalized = lexical_normalize(&resolved);
        return if normalized.starts_with(root) {
            LinkClass::Internal
        } else {
            LinkClass::DanglingExternal
        };
    };
    if real.starts_with(root) {
        LinkClass::Internal
    } else if real.is_dir() {
        LinkClass::ExternalDir
    } else {
        // A canonicalized path is a regular file (or a special, which
        // behaves like a file for classification purposes).
        LinkClass::ExternalFile
    }
}

/// Resolve a link's literal `target` against `link_path`'s parent directory
/// (absolute targets pass through). Purely lexical — the target may not
/// exist. Shared with the scanner, which needs the same resolution to
/// compute a rewritten target.
pub(crate) fn resolve_target(link_path: &Path, target: &str) -> PathBuf {
    let t = Path::new(target);
    if t.is_absolute() {
        t.to_path_buf()
    } else {
        link_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(t)
    }
}

/// Normalize `.`/`..` components lexically (no filesystem access), used for
/// classifying dangling targets and detecting self-referential links. A
/// `..` cancels the previous normal component; at the filesystem root (or a
/// bare drive prefix on Windows) it is a no-op — it can never climb above
/// the root. Rebuilds the path from the surviving components so drive
/// prefixes and root separators are preserved correctly.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // `..` cancels the previous normal component; above the root
                // (RootDir or a bare drive Prefix) it is a no-op.
                if matches!(parts.last(), Some(Component::Normal(_))) {
                    parts.pop();
                }
            }
            Component::CurDir => {}
            other => parts.push(other),
        }
    }
    let mut out = PathBuf::new();
    for component in parts {
        out.push(component.as_os_str());
    }
    out
}

/// The first four bytes of every Shell Link (.lnk) file — the
/// `ShellLinkHeader.HeaderSize` field, fixed at `0x0000004C` (MS-SHLLINK).
/// Used to sniff whether a file named `*.lnk` is really a shortcut before
/// handing it to the parser (spec §3.2). Platform-free: the magic is the
/// same on every OS.
#[must_use]
pub fn is_lnk_magic(data: &[u8]) -> bool {
    data.len() >= 4 && u32::from_le_bytes([data[0], data[1], data[2], data[3]]) == 0x0000_004C
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_hint_matches_script_and_binary_extensions() {
        for ext in ["exe", "bat", "cmd", "ps1", "sh", "pl", "py", "rb", "lua"] {
            assert!(compute_exec_hint(Path::new(&format!("tool.{ext}"))), "{ext}");
            // Case-insensitive.
            assert!(
                compute_exec_hint(Path::new(&format!("tool.{}", ext.to_uppercase()))),
                "{ext} uppercase"
            );
        }
        assert!(!compute_exec_hint(Path::new("notes.txt")));
        assert!(!compute_exec_hint(Path::new("noext")));
        assert!(!compute_exec_hint(Path::new("dir/")));
    }

    #[test]
    fn final_mode_unix_source_keeps_bits_clears_high_bits() {
        // Unix → Unix: rwxrwxrwx preserved, SUID/SGID/Sticky force-cleared.
        assert_eq!(final_mode(TargetOs::Unix, TargetOs::Unix, 0o4755, false, false, false, false), 0o755);
        assert_eq!(final_mode(TargetOs::Unix, TargetOs::Unix, 0o2750, false, false, false, false), 0o750);
        assert_eq!(final_mode(TargetOs::Unix, TargetOs::Unix, 0o1777, true, false, false, false), 0o777);
        assert_eq!(final_mode(TargetOs::Unix, TargetOs::Unix, 0o640, false, false, false, false), 0o640);
    }

    #[test]
    fn final_mode_archive_keeps_suid_sgid_sticky() {
        // `-a` (archive): byte-identical mode keeps SUID/SGID/Sticky.
        assert_eq!(final_mode(TargetOs::Unix, TargetOs::Unix, 0o4755, false, false, false, true), 0o4755);
        assert_eq!(final_mode(TargetOs::Unix, TargetOs::Unix, 0o2750, false, false, false, true), 0o2750);
        assert_eq!(final_mode(TargetOs::Unix, TargetOs::Unix, 0o1777, true, false, false, true), 0o1777);
        // `--no-perms` still wins over the archive bits.
        assert_eq!(final_mode(TargetOs::Unix, TargetOs::Unix, 0o4755, false, false, true, true), 0o644);
    }

    #[test]
    fn final_mode_unix_source_no_perms_explicit_defaults() {
        // `--no-perms`: explicit 0644/0755 regardless of the source bits.
        assert_eq!(final_mode(TargetOs::Unix, TargetOs::Unix, 0o600, false, false, true, false), 0o644);
        assert_eq!(final_mode(TargetOs::Unix, TargetOs::Unix, 0o777, true, false, true, false), 0o755);
    }

    #[test]
    fn final_mode_windows_source_uses_exec_hint() {
        assert_eq!(final_mode(TargetOs::Windows, TargetOs::Unix, 0, false, true, false, false), 0o755);
        assert_eq!(final_mode(TargetOs::Windows, TargetOs::Unix, 0, false, false, false, false), 0o644);
        assert_eq!(final_mode(TargetOs::Windows, TargetOs::Unix, 0, true, false, false, false), 0o755);
        // `--no-perms` disables the heuristic: every file 0644.
        assert_eq!(final_mode(TargetOs::Windows, TargetOs::Unix, 0, false, true, true, false), 0o644);
    }

    #[test]
    fn final_mode_ignores_target_os() {
        // The value is the same for any target; the receiver applies (Unix)
        // or ignores (Windows) it.
        assert_eq!(
            final_mode(TargetOs::Unix, TargetOs::Windows, 0o640, false, false, false, false),
            0o640
        );
    }

    #[test]
    fn rel_path_descends_and_ascends() {
        assert_eq!(rel_path(Path::new("a"), Path::new("a/t.txt")), PathBuf::from("t.txt"));
        assert_eq!(rel_path(Path::new("a/b"), Path::new("a/c.txt")), PathBuf::from("../c.txt"));
        assert_eq!(rel_path(Path::new("a/b"), Path::new("d/e.txt")), PathBuf::from("../../d/e.txt"));
        assert_eq!(rel_path(Path::new("a"), Path::new("a")), PathBuf::from("."));
        assert_eq!(rel_path(Path::new(""), Path::new("x.txt")), PathBuf::from("x.txt"));
    }

    #[test]
    fn rewrite_internal_target_is_dest_relative() {
        // Link at `a/link.txt` → target `a/sub/t.txt`: relative = `sub/t.txt`.
        assert_eq!(rewrite_internal_target("a/link.txt", "a/sub/t.txt"), "sub/t.txt");
        // Link at `links/l` → target `links/../shared/t`: relative `../shared/t`.
        assert_eq!(rewrite_internal_target("links/l", "shared/t"), "../shared/t");
        // Same directory: bare name.
        assert_eq!(rewrite_internal_target("link.txt", "target.txt"), "target.txt");
        // Top-level link → nested target.
        assert_eq!(rewrite_internal_target("l", "sub/x"), "sub/x");
    }

    #[cfg(unix)]
    #[test]
    fn classify_link_internal_relative_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("sub/t.txt");
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink("sub/t.txt", root.path().join("link.txt")).unwrap();
        let root_canonical = root.path().canonicalize().unwrap();
        let link = root.path().join("link.txt");
        assert_eq!(
            classify_link(&link, "sub/t.txt", &root_canonical),
            LinkClass::Internal
        );
    }

    #[cfg(unix)]
    #[test]
    fn classify_link_absolute_target_inside_root_is_internal() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("t.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(root.path().join("t.txt"), root.path().join("abs.txt")).unwrap();
        let root_canonical = root.path().canonicalize().unwrap();
        let abs_target = root_canonical.join("t.txt");
        assert_eq!(
            classify_link(
                &root.path().join("abs.txt"),
                &abs_target.to_string_lossy(),
                &root_canonical
            ),
            LinkClass::Internal
        );
    }

    #[cfg(unix)]
    #[test]
    fn classify_link_external_file_and_dir() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("f.txt"), b"x").unwrap();
        std::fs::create_dir(outside.path().join("d")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("f.txt"), root.path().join("f_link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("d"), root.path().join("d_link")).unwrap();
        let root_canonical = root.path().canonicalize().unwrap();
        assert_eq!(
            classify_link(&root.path().join("f_link"), &outside.path().join("f.txt").to_string_lossy(), &root_canonical),
            LinkClass::ExternalFile
        );
        assert_eq!(
            classify_link(&root.path().join("d_link"), &outside.path().join("d").to_string_lossy(), &root_canonical),
            LinkClass::ExternalDir
        );
    }

    #[cfg(unix)]
    #[test]
    fn classify_link_dangling_external_and_internal() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // Dangling external: target path outside the root, does not exist.
        let dangling_out = outside.path().join("missing.txt");
        std::os::unix::fs::symlink(&dangling_out, root.path().join("dangling_out")).unwrap();
        let root_canonical = root.path().canonicalize().unwrap();
        assert_eq!(
            classify_link(&root.path().join("dangling_out"), &dangling_out.to_string_lossy(), &root_canonical),
            LinkClass::DanglingExternal
        );
        // Dangling internal: target path inside the root, does not exist yet
        // — still preserved as an internal link.
        std::os::unix::fs::symlink("not-yet.txt", root.path().join("dangling_in")).unwrap();
        assert_eq!(
            classify_link(&root.path().join("dangling_in"), "not-yet.txt", &root_canonical),
            LinkClass::Internal
        );
    }

    #[test]
    fn lnk_magic_sniffs_header_size() {
        assert!(is_lnk_magic(&[0x4C, 0x00, 0x00, 0x00, 0x01]));
        assert!(is_lnk_magic(&[0x4C, 0x00, 0x00, 0x00]));
        assert!(!is_lnk_magic(&[0x4C, 0x00, 0x00]));
        assert!(!is_lnk_magic(b"LNK1"));
        assert!(!is_lnk_magic(b""));
    }

    #[test]
    fn lexical_normalize_cancels_parent_dirs() {
        assert_eq!(lexical_normalize(Path::new("a/b/../c")), PathBuf::from("a/c"));
        assert_eq!(lexical_normalize(Path::new("a/./b")), PathBuf::from("a/b"));
        assert_eq!(lexical_normalize(Path::new("a/..")), PathBuf::new());
        // `..` cannot climb above the root.
        assert_eq!(lexical_normalize(Path::new("/../x")), PathBuf::from("/x"));
        assert_eq!(lexical_normalize(Path::new("/a/../../x")), PathBuf::from("/x"));
        // A semantic self-reference normalizes to the link path itself.
        assert_eq!(
            lexical_normalize(Path::new("/root/sub/../sub/l")),
            PathBuf::from("/root/sub/l")
        );
    }

    #[cfg(windows)]
    #[test]
    fn lexical_normalize_keeps_drive_root() {
        // `..` from a drive root stays at the root; the separator survives.
        assert_eq!(
            lexical_normalize(Path::new(r"C:\..\x")),
            PathBuf::from(r"C:\x")
        );
        assert_eq!(
            lexical_normalize(Path::new(r"C:\a\..\x")),
            PathBuf::from(r"C:\x")
        );
    }

    #[test]
    fn target_os_from_os_name() {
        assert_eq!(TargetOs::from_os_name("windows"), TargetOs::Windows);
        assert_eq!(TargetOs::from_os_name("Windows"), TargetOs::Windows);
        assert_eq!(TargetOs::from_os_name("linux"), TargetOs::Unix);
        assert_eq!(TargetOs::from_os_name("macos"), TargetOs::Unix);
        assert_eq!(TargetOs::from_os_name("darwin"), TargetOs::Unix);
    }
}
