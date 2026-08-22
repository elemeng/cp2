# cp2 roadmap

## v0.1 — current

Released feature set (see `README.md` for details):

- Rsync-style sync over SSH with auto-deploy, one-password-per-run multiplexing
  on Unix and a pure-Rust russh transport on Windows (one connection, one
  channel per session),
  and rsync path semantics (absolute/relative/serve-root).
- FastCDC/BLAKE3 delta engine: changed-chunks-only updates, chunked streaming
  of large files, `--verify` and hash-guarded `--remove-source-files`.
- Full rsync `-a` on Unix (special files/devices; owner/group are never
  preserved — 0-Root), `rlpt` by default with
  `--no-*` opt-outs, `--delete`, `--backup`, `--watch`, glob sources,
  `--files-from` (absolute-path lists), single-file sources.
- Link policy (spec §2/§3): internal links rewritten to DEST-relative
  targets, external file links dereferenced by default, external directory
  links skipped by default, with the fine-grained `--literal-*` switches
  keeping each class literal, `--literal-links` (implied by `-a`) keeping
  every link's literal target string, `--follow-links` dereferencing
  everything (loop-detected), and `--skip-links` skipping everything;
  Windows-source `.lnk` shortcuts are magic-sniffed and materialized as
  `.lnk`/symlink entries (or copied as opaque files under `--literal-links`).
- Cross-platform: Linux, macOS, Windows.

## v0.2 — planned

### Content-addressed dedup (`--dedup-host-ref DIR`) — headline

Unified whole-file + chunk dedup against the destination's existing content
(the design from the v0.1 discussions: levels 1 and 2a are one mechanism —
content-addressed chunk transfer).

- **Basis pool**: a reference directory on the destination host
  (`--dedup-host-ref`, rsync `--link-dest` semantics) and/or the destination
  tree itself.
- **Whole-file match** (level 1): source chunk set equals one existing file's
  → hard link with path/metadata updated; no content over the wire. Saves
  space and bandwidth (renamed, touched, and duplicated files).
- **Partial match** (level 2a): delta against the best-matching basis by
  chunk-hash overlap — only the missing chunks travel; receiver reconstructs
  the full file. Saves bandwidth for version series (`app-v0.1.1.iso` →
  `app-v0.1.2.iso`).
- **Engine**: extend basis selection from "same path's previous version" to
  "best-matching file in the reference pool"; reuse the existing FastCDC
  chunking, BLAKE3 identity, Copy/Literal ops, and reconstruction.
- **Cost model**: quick check runs first and free; only quick-check-failed
  files enter the chunk path; one batched signature round-trip; one read per
  side. The reference tree is treated as immutable (hard links share the
  inode).
- **Explicit non-goal**: on-disk chunk stores (borg/restic-style shared
  chunks) — that restructures the destination and is out of scope for a
  mirroring sync tool.

## v0.3 — planned

### Bidirectional sync — headline

Merge two trees both ways in one run (the unison model): changes on either
side propagate to the other, with no fixed source/destination roles.

- **Two-pass**: scan both trees, compute the change sets in each direction,
  then transfer both ways over one session.
- **Conflict policy**: a file changed on both sides needs a rule — default
  last-write-wins (by mtime), keep-both (conflict copy, unison-style), or
  explicit per-file resolution; the policy is a v0.3 design decision.
- **Deletions**: propagate both directions (tombstone-aware, so a deletion
  on one side isn't resurrected by a stale copy on the other).
- **Reference model**: unison; rsync `--update` is one-directional and not a
  substitute.
- Builds on the existing protocol (the pull side already runs a full
  sender/receiver session; bidirectional is two half-sessions merged into
  one plan).

### Candidates (known gaps, not yet committed)

- Content-addressed dedup is the v0.2 headline above; bidirectional sync the
  v0.3 headline. The previous candidates (metadata-only updates; remote-side
  expansion for globs and `--files-from` on pull) are done — see the released
  list: attr-only re-apply for drifted perms/times, remote `--list-only`,
  server-expanded globs on pull, and remote `--files-from`.
