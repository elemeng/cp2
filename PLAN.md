# PLAN — path handling hardening (soft-canonicalize + dunce, typed-path)

Goal: make cp2's Windows/Linux path handling more robust and explicit. Two
localized changes, no behavior regressions, verified by the existing
containment and wire tests.

## Change 1 — `soft-canonicalize` (+ `dunce`) in the path sanitizer

**File:** `src/security/path_sanitizer.rs`

This is the security-critical canonicalization layer (chroot-like containment
on the receiver). It currently hand-rolls a "canonicalize the nearest existing
ancestor" walk; `soft-canonicalize` does exactly that in one call.

- `soft-canonicalize` resolves the deepest **existing** prefix (with symlink +
  `..` + cycle handling, `MAX_SYMLINK_DEPTH`-bounded) and appends the
  non-existing tail. That removes the manual parent-up loop.
- The `dunce` feature cleans the Windows `\\?\` extended-length prefix to
  `C:\foo` **only when safe** (keeps the UNC form for >260-char paths, reserved
  names, trailing dots/spaces, literal `..`), fixing the `\\?\` artifact that
  currently leaks into canonical paths on Windows.
- `proc-canonicalize` (default feature) preserves the Linux namespace boundary
  for magic symlinks like `/proc/self/root`.

Edits:
- `PathSanitizer::new`: `root.as_ref().canonicalize()?` → `soft_canonicalize(root)?`.
  Using the same canonicalizer for the root and for every join keeps both sides
  in the same (dunce-clean) format, so `starts_with` containment stays correct.
- `ensure_within_root`: replace the `loop { std::fs::canonicalize(current) … }`
  walk with a single `soft_canonicalize(path)` call followed by the existing
  `real.starts_with(&self.root)` guard. Keep the guard — **the containment
  decision stays cp2's**; the crate only provides the resolved path.
- Keep the deliberate per-join re-canonicalization (no caching): the receiver
  mutates the tree via the protocol, so cached results can go stale. The crate
  does not compromise this.

Verification: `cargo test --lib security::path_sanitizer` — the `#[cfg(unix)]`
symlink-escape and rechecks-after-replacement tests must still pass; the
Windows-path tests must pass on Windows too.

## Change 2 — `typed-path` in the wire / link-target construction layer

**Files:** `src/sync/wire.rs`, `src/sync/linkpolicy.rs`, `src/sync/scanner.rs`

Give the wire-relative (`/`-separated, host-independent) paths explicit Unix
semantics instead of ad-hoc separator strings, so a Windows build can't leak
`\` semantics into the wire form.

- `src/sync/wire.rs`:
  - `wire_str` / `wire_rel`: express the `/`-normalization as a `Utf8UnixPath`
    round-trip instead of raw `replace('\\', "/")` string surgery, making the
    `/` contract a type-level property.
  - Keep `file_meta_from_entry` / `manifest_from_file_meta` behavior identical.
- `src/sync/linkpolicy.rs`:
  - `rel_path` and `rewrite_internal_target` (DEST-relative link-target
    building): operate with `Utf8UnixPath`/`Utf8UnixPathBuf` so the relative /
    `..` /`/`-join logic is Unix-semantic on every host (no dependence on the
    compilation target's `std::path` rules).
- `src/sync/scanner.rs`: the `include_root_component` prefix currently builds
  `format!("{name}/{}", …)`; express it via `Utf8UnixPathBuf` push for the same
  host-independent reason.

Scope note: the local-filesystem side (open/readdir/stat) stays on host-native
`std::path::Path`; `typed-path` is only for the wire/relative-path surface. The
serialized wire format remains plain `/` strings — typed-path is internal.

Verification: `cargo test --lib sync::wire`, `sync::linkpolicy`,
`sync::scanner` (including `include_root_component_helper` and the root-prefix
test) and `cargo clippy` (no new lint on the touched files; the 4 pre-existing
Windows-only clippy errors are unrelated).

## Out of scope (assessed, not adopted)
- `canonical-path` (v2) — canonical-path *newtypes* (`CanonicalPath/Buf`); a
  type-level hygiene nicety, not a behavior fix. Skipped.
- `sugar-path` — host-native lexical sugar, no canonicalization, overlaps the
  small existing helpers. Skipped.

## Dependency additions (`Cargo.toml`)
- `soft-canonicalize = { version = "0.5", features = ["dunce"] }` (keeps
  default `proc-canonicalize`).
- `typed-path = "0.12"`.
