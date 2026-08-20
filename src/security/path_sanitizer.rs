//! Path sanitizer to prevent directory traversal attacks

use std::path::{Component, Path, PathBuf};

use soft_canonicalize::soft_canonicalize;

use super::error::{SecurityError, SecurityResult};

/// Validate the components of a peer-supplied relative path: reject empty,
/// absolute (root/drive-prefixed), and parent-traversal (`..`) forms.
fn validate_components(user_path: &str) -> SecurityResult<PathBuf> {
    if user_path.is_empty() {
        return Err(SecurityError::AbsolutePathNotAllowed);
    }
    let path = Path::new(user_path);
    for component in path.components() {
        match component {
            Component::ParentDir => return Err(SecurityError::TraversalAttempt),
            Component::RootDir | Component::Prefix(_) => {
                return Err(SecurityError::AbsolutePathNotAllowed);
            }
            // `CurDir` (`.`) and `Normal` components are safe.
            _ => {}
        }
    }
    Ok(path.to_path_buf())
}

/// Path sanitizer with chroot-like enforcement.
///
/// Used by the receiver to join a peer-supplied relative path under a root
/// directory while rejecting absolute paths and `..` traversal — including
/// traversal through symlinks that resolve outside the root.
///
/// The symlink check canonicalizes the nearest existing ancestor on every
/// join. It is deliberately **not** cached: the receiver itself mutates the
/// tree through the protocol (a peer can replace a verified directory with a
/// symlink via `CreateLinks`), so a cached verification would go stale and
/// let a later join escape the root through the new symlink.
pub struct PathSanitizer {
    root: PathBuf,
}

impl PathSanitizer {
    /// Create new path sanitizer with root directory.
    ///
    /// The root is canonicalized so later containment checks compare
    /// real paths.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the root cannot be canonicalized.
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let root = soft_canonicalize(root)?;
        Ok(PathSanitizer { root })
    }

    /// Join a user-provided relative path under root without requiring the
    /// target to already exist.
    ///
    /// Rejects absolute paths, drive prefixes, and any `..` component.
    /// Legitimate names containing `..` as a substring (e.g. `a..b`) are
    /// allowed. The parent directory chain is verified to resolve inside the
    /// root, so a symlink inside the tree cannot redirect a write outside it
    /// (the file itself is never followed — it is replaced via a staged temp).
    ///
    /// # Errors
    ///
    /// Returns a [`SecurityError`] if the path is absolute, contains a drive
    /// prefix or `..` component, or its parent chain resolves outside root.
    pub fn join(&self, user_path: &str) -> SecurityResult<PathBuf> {
        let path = validate_components(user_path)?;
        let joined = self.root.join(path);
        let parent = joined.parent().unwrap_or(&self.root);
        self.ensure_within_root(parent)?;
        Ok(joined)
    }

    /// Join a user-provided relative path under the root when the caller has
    /// already verified every parent via [`Self::verify_parent`] (the batch
    /// apply verifies each unique parent directory once — the per-join
    /// canonicalization walk is the dominant per-file cost on small-file
    /// trees). The component validation still runs; the parent-chain walk is
    /// skipped. Safe within one batch task: the receiver's own mutations
    /// cannot interleave (the "no caching" rule in the struct docs is about
    /// cross-frame interleaving, where a peer can replace a verified
    /// directory with a symlink).
    pub(crate) fn join_preverified(&self, user_path: &str) -> SecurityResult<PathBuf> {
        let path = validate_components(user_path)?;
        Ok(self.root.join(path))
    }

    /// Verify that `path`'s nearest existing ancestor canonicalizes within
    /// the sanitizer root — the batch apply calls this once per unique
    /// parent directory before joining its files with
    /// [`Self::join_preverified`].
    pub(crate) fn verify_parent(&self, path: &Path) -> SecurityResult<()> {
        self.ensure_within_root(path)
    }

    /// Verify the nearest existing ancestor of `path` canonicalizes within
    /// the sanitizer root, so symlinks inside the tree cannot escape it.
    ///
    /// Runs on every join — no caching (see the struct docs: the receiver's
    /// own mutations can make a cached result stale). `soft_canonicalize`
    /// resolves the deepest existing prefix (symlinks, `..`, cycles, all
    /// `MAX_SYMLINK_DEPTH`-bounded) and appends the non-existing tail, so the
    /// containment decision below compares real paths on both sides.
    fn ensure_within_root(&self, path: &Path) -> SecurityResult<()> {
        let real = soft_canonicalize(path).map_err(|_| SecurityError::TraversalAttempt)?;
        if !real.starts_with(&self.root) {
            return Err(SecurityError::TraversalAttempt);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_join_rejects_absolute() {
        let temp = TempDir::new().unwrap();
        let sanitizer = PathSanitizer::new(temp.path()).unwrap();
        assert!(matches!(
            sanitizer.join("/etc/passwd"),
            Err(SecurityError::AbsolutePathNotAllowed)
        ));
    }

    #[test]
    fn test_join_rejects_traversal() {
        let temp = TempDir::new().unwrap();
        let sanitizer = PathSanitizer::new(temp.path()).unwrap();
        assert!(matches!(
            sanitizer.join("../../etc/passwd"),
            Err(SecurityError::TraversalAttempt)
        ));
        assert!(matches!(
            sanitizer.join("sub/../escape.txt"),
            Err(SecurityError::TraversalAttempt)
        ));
        assert!(matches!(
            sanitizer.join("a/b/../../escape.txt"),
            Err(SecurityError::TraversalAttempt)
        ));
    }

    #[test]
    fn test_join_allows_valid_relative_path() {
        let temp = TempDir::new().unwrap();
        let sanitizer = PathSanitizer::new(temp.path()).unwrap();
        let result = sanitizer.join("subdir/file.txt");
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with(temp.path()));
    }

    #[test]
    fn test_join_allows_dots_in_names() {
        let temp = TempDir::new().unwrap();
        let sanitizer = PathSanitizer::new(temp.path()).unwrap();
        // `..` as a substring of a name is not traversal.
        assert!(sanitizer.join("a..b.txt").is_ok());
        assert!(sanitizer.join("sub/..hidden").is_ok());
        // A bare `.` component is harmless.
        assert!(sanitizer.join("sub/./file.txt").is_ok());
    }

    #[test]
    fn test_join_rejects_empty() {
        let temp = TempDir::new().unwrap();
        let sanitizer = PathSanitizer::new(temp.path()).unwrap();
        assert!(matches!(
            sanitizer.join(""),
            Err(SecurityError::AbsolutePathNotAllowed)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_join_rejects_symlink_escape() {
        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("escaped.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join("link")).unwrap();

        let sanitizer = PathSanitizer::new(temp.path()).unwrap();
        // The path exists under the link target, so canonicalization resolves
        // it outside the root and it is rejected.
        assert!(matches!(
            sanitizer.join("link/escaped.txt"),
            Err(SecurityError::TraversalAttempt)
        ));
        // Non-escaping symlinks inside the root are fine.
        std::os::unix::fs::symlink(temp.path().join("sub"), temp.path().join("link2")).unwrap();
        std::fs::create_dir(temp.path().join("sub")).unwrap();
        assert!(sanitizer.join("link2/file.txt").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn test_join_rechecks_after_dir_replaced_by_symlink() {
        // Regression: a directory verified once must not stay "trusted" if it
        // is later replaced by a symlink (the receiver can do exactly this via
        // a `CreateLinks` frame) — a stale cache would let a join escape the
        // root through the new symlink.
        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let sanitizer = PathSanitizer::new(temp.path()).unwrap();

        // Verify a real subdirectory (the old code cached it here).
        std::fs::create_dir(temp.path().join("a")).unwrap();
        assert!(sanitizer.join("a/b.txt").is_ok());

        // The peer replaces the directory with a symlink pointing outside.
        std::fs::remove_dir(temp.path().join("a")).unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join("a")).unwrap();

        // A join through the now-symlinked parent must be rejected.
        assert!(matches!(
            sanitizer.join("a/c.txt"),
            Err(SecurityError::TraversalAttempt)
        ));
    }
}
