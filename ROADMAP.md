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

## Planned

No forward-looking features are currently committed. The items previously
tracked here (content-addressed dedup `--dedup-host-ref`, bidirectional sync)
are off the roadmap; the earlier candidates — metadata-only attribute
re-apply and remote-side expansion for globs/`--files-from` on pull — are
done, see the released list (attr-only re-apply for drifted perms/times,
remote `--list-only`, server-expanded globs on pull, remote `--files-from`).