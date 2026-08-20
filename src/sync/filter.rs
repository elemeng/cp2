//! Rsync-style include/exclude filters for the scanner.
//!
//! Pure decision logic: a [`FilterSet`] decides whether a relative path should
//! be synced. Glob semantics (documented in [`FilterSet::passes`]):
//!
//! - `*` matches any run of characters (including `/`); `**` is the same.
//! - `?` matches exactly one character.
//! - A pattern with a leading `/` is anchored at the scan root.
//! - A pattern containing `/` (without a leading slash) matches at any depth.
//! - A pattern with no `/` matches against the file/directory basename.
//! - Includes override excludes: any include beats any exclude, regardless
//!   of the order given (a deliberate simplification of rsync's
//!   order-sensitive first-match-wins).

#![forbid(unsafe_code)]

/// A set of include/exclude glob patterns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterSet {
    /// Paths matching any include are always synced (override excludes).
    pub includes: Vec<String>,
    /// Paths matching any exclude are skipped.
    pub excludes: Vec<String>,
}

impl FilterSet {
    /// Whether any filter is configured.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.includes.is_empty() || !self.excludes.is_empty()
    }

    /// Whether the relative path `path` (slash-separated) passes the filter.
    #[must_use]
    pub fn passes(&self, path: &str) -> bool {
        if !self.is_active() {
            return true;
        }
        let basename = path.rsplit('/').next().unwrap_or(path);
        let matched =
            |patterns: &[String]| patterns.iter().any(|p| pattern_matches(p, path, basename));
        if matched(&self.includes) {
            return true;
        }
        if matched(&self.excludes) {
            return false;
        }
        true
    }
}

/// Whether `pattern` matches `path`, using `basename` for slashless patterns.
fn pattern_matches(pattern: &str, path: &str, basename: &str) -> bool {
    let anchored = pattern.starts_with('/');
    let pattern = pattern.trim_start_matches('/').trim_end_matches('/');
    // `**/x` means "x at any depth", including the root (no preceding slash).
    let pattern = pattern.strip_prefix("**/").unwrap_or(pattern);
    if pattern.is_empty() {
        return false;
    }
    if pattern.contains('/') {
        let pat = pattern.as_bytes();
        if anchored {
            // Anchored slashed pattern: matches a root-relative prefix.
            prefix_matches(pat, path.as_bytes())
        } else if has_wildcards(pat) {
            // Match at any depth: try every suffix of the path.
            let bytes = path.as_bytes();
            (0..=bytes.len()).any(|start| glob(pat, &bytes[start..]))
        } else {
            // Literal slashed pattern: a single `contains` scan instead of
            // a glob per suffix.
            path.as_bytes().windows(pat.len()).any(|w| w == pat)
        }
    } else if anchored {
        // Anchored slashless pattern: the named root entry and its subtree.
        prefix_matches(pattern.as_bytes(), path.as_bytes())
    } else {
        glob(pattern.as_bytes(), basename.as_bytes())
    }
}

/// Whether `pattern` contains any wildcard character.
fn has_wildcards(pattern: &[u8]) -> bool {
    pattern.iter().any(|&b| b == b'*' || b == b'?')
}

/// Whether `pattern` matches a root-relative prefix of `path`: either the full
/// path, or the pattern followed by a `/` (i.e. it names a directory whose
/// contents are below `path`).
fn prefix_matches(pattern: &[u8], path: &[u8]) -> bool {
    if glob(pattern, path) {
        return true;
    }
    path.starts_with(pattern) && path.get(pattern.len()) == Some(&b'/')
}

/// Match `pattern` (with `*` and `?` wildcards) against `text` as a full
/// match — the classic linear wildcard matcher (greedy `*` backtracking):
/// O(pattern + text) time, no allocation, instead of a per-call dynamic
/// program table (the scanner runs this per entry, so the table churn was
/// measurable on large trees with filters).
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    if !has_wildcards(pattern) {
        return pattern == text;
    }
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (usize::MAX, 0usize);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            // Remember the star and try matching it with the empty run.
            star_p = p;
            star_t = t;
            p += 1;
        } else if star_p != usize::MAX {
            // Mismatch after a star: extend the star's matched run by one.
            p = star_p + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(excludes: &[&str], includes: &[&str]) -> FilterSet {
        FilterSet {
            excludes: excludes
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            includes: includes
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
        }
    }

    #[test]
    fn inactive_passes_everything() {
        let f = FilterSet::default();
        assert!(f.passes("a/b/c.txt"));
    }

    #[test]
    fn basename_pattern_matches_at_any_depth() {
        let f = set(&["*.tmp"], &[]);
        assert!(!f.passes("a.tmp"));
        assert!(!f.passes("sub/b.tmp"));
        assert!(f.passes("sub/b.txt"));
    }

    #[test]
    fn question_mark_matches_single_char() {
        let f = set(&["?x"], &[]);
        assert!(!f.passes("ax"));
        assert!(f.passes("aax"));
    }

    #[test]
    fn anchored_pattern_is_root_only() {
        let f = set(&["/build"], &[]);
        assert!(!f.passes("build"));
        assert!(!f.passes("build/x.rs"));
        assert!(f.passes("sub/build"));
    }

    #[test]
    fn slashed_pattern_matches_at_any_depth() {
        let f = set(&["src/*.rs"], &[]);
        assert!(!f.passes("src/main.rs"));
        assert!(!f.passes("a/b/src/main.rs"));
        assert!(f.passes("src/main.c"));
    }

    #[test]
    fn includes_override_excludes() {
        let f = set(&["*.tmp"], &["keep.tmp"]);
        assert!(!f.passes("other.tmp"));
        assert!(f.passes("keep.tmp"));
        assert!(f.passes("other.txt"));
    }

    #[test]
    fn wildcard_matches_across_slashes() {
        let f = set(&["**/generated"], &[]);
        assert!(!f.passes("generated"));
        assert!(!f.passes("a/generated"));
        assert!(!f.passes("a/b/generated"));
        assert!(f.passes("a/b/keep"));
    }

    #[test]
    fn glob_star_backtracking_edge_cases() {
        // The linear matcher's greedy `*` backtracking must be exact for
        // the `*`/`?` language (a table-based DP previously handled it).
        for (pat, text) in [
            ("a*b", "ab"),        // star matches empty
            ("a*b", "axxb"),      // star matches a run
            ("*a", "ba"),         // leading star
            ("a*", "a"),          // trailing star, empty run
            ("*a*", "ba"),        // two stars, one empty
            ("*a*b*", "xaybz"),   // alternating
            ("a*b*c", "abc"),     // adjacent stars
            ("*?", "x"),          // star then literal
            ("*a?b*", "zzaybzz"), // star + ? + literal + star
        ] {
            assert!(glob(pat.as_bytes(), text.as_bytes()), "{pat} ~ {text}");
        }
        for (pat, text) in [
            ("a*c", "ab"),     // no room for the tail
            ("*?", ""),        // `?` needs a character
            ("?x", "xy"),      // `?` consumed the first char, literal must follow
            ("a*b", "ba"),     // order matters
            ("ab", "abc"),     // full match only
            ("ab", "aab"),     // full match only
        ] {
            assert!(!glob(pat.as_bytes(), text.as_bytes()), "{pat} !~ {text}");
        }
    }
}
