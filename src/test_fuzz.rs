//! Deterministic, dependency-free input generators for `#[cfg(test)]` fuzz
//! targets.
//!
//! The security-critical surfaces — the frame codec, the path sanitizer, and
//! the remote-glob resolver — are exercised with randomized inputs from a
//! seeded PRNG instead of the `rand` crate: every `cargo test` replay replays
//! fixed seeds, so a decode or containment regression is caught on the next
//! run with no fuzz infrastructure. The generators below are deliberately
//! biased toward the adversarial shapes those surfaces parse: length-prefixed
//! byte buffers, traversal and dotfile tokens, glob metacharacters, and
//! absolute-path forms.

/// xorshift64* — a tiny, deterministic PRNG, plenty for test input
/// generation (modulo bias is irrelevant here).
pub struct FuzzRng(u64);

impl FuzzRng {
    /// A generator seeded `seed` (zero is a fixed point of xorshift, so a
    /// constant is mixed in).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// An index in `0..n`.
    #[must_use]
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "below(0) is meaningless");
        let n64 = u64::try_from(n).unwrap_or(u64::MAX);
        usize::try_from(self.next() % n64).unwrap_or(usize::MAX)
    }

    /// A random byte.
    #[must_use]
    pub fn byte(&mut self) -> u8 {
        u8::try_from(self.next() % 256).unwrap_or(u8::MAX)
    }

    /// A random byte buffer of `0..=max_len` bytes.
    #[must_use]
    pub fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let len = self.below(max_len + 1);
        (0..len).map(|_| self.byte()).collect()
    }

    /// A random string from a path-ish charset: separators, glob
    /// metacharacters, dot tokens, spaces, unicode, and NUL.
    #[must_use]
    pub fn string(&mut self, max_len: usize) -> String {
        const CHARS: &[char] = &[
            'a', 'b', 'Z', '0', ' ', '_', '-', '.', '/', '\\', '*', '?', '[', ']', '(', ')',
            '=', 'é', '中', '\0', ':', '|', '~', '\'', '"', '{', '}',
        ];
        let len = self.below(max_len + 1);
        let mut s = String::with_capacity(len);
        for _ in 0..len {
            s.push(CHARS[self.below(CHARS.len())]);
        }
        s
    }

    /// A path-like string of `0..=max_segments` realistic segments joined by
    /// `/` or `\`, occasionally with a leading separator (absolute-path
    /// shapes) — the traversal and glob forms random char soup rarely
    /// produces on its own.
    #[must_use]
    pub fn pathish(&mut self, max_segments: usize) -> String {
        const SEGMENTS: &[&str] = &[
            "a",
            "b",
            "sub",
            "name with space",
            "é",
            "..",
            ".",
            "*",
            "?.bin",
            "[0-9]",
            "x..y",
            ".hidden",
            "backup",
            "a?b*c",
            "]",
            "",
            "dir/other",
        ];
        const SEPS: &[char] = &['/', '\\'];
        let n = self.below(max_segments + 1);
        let mut out = String::new();
        for i in 0..n {
            if i > 0 {
                out.push(SEPS[self.below(SEPS.len())]);
            }
            out.push_str(SEGMENTS[self.below(SEGMENTS.len())]);
        }
        if self.below(8) == 0 {
            out.insert(0, SEPS[self.below(SEPS.len())]);
        }
        out
    }
}