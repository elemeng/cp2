# cp2

[![Crates.io](https://img.shields.io/crates/v/cp2.svg)](https://crates.io/crates/cp2)
[![docs.rs](https://docs.rs/cp2/badge.svg)](https://docs.rs/cp2)
[![License](https://img.shields.io/crates/l/cp2.svg)](https://crates.io/crates/cp2)

**A pure-Rust, high-performance copy & sync tool** — a modern `cp` and
`rsync`-like — for local directories or over the network, on Linux, macOS,
and Windows.

Copy files locally, or sync two machines over SSH with rsync-style
semantics — but with a modern delta engine that sends only the bytes that
actually changed, verification you can trust, and **zero server setup**. One
binary, pure Rust, no runtime dependencies beyond the `ssh` client you
already have.

## Why cp2?

- **Sends only what changed** — FastCDC content-defined chunking + BLAKE3: a
  1-byte edit in a 50 GB file transfers kilobytes, not gigabytes, and large
  files stream in bounded memory (a 100 GB file never needs 100 GB of RAM).
- **Zero-setup server** — cp2 deploys a matching binary to the remote
  automatically on first sync. `cp2 SRC user@host:DEST` just works against a
  fresh account: no install, no sudo, no PATH setup.
- **One password prompt** — on Unix the run's ssh sessions multiplex over a
  single `ControlMaster` connection, so with password auth you authenticate
  once per run. On Windows (where OpenSSH's multiplexing socket is broken,
  `getsockname failed: Not a socket`) cp2 uses its own pure-Rust SSH client
  (`russh`): one connection, one authentication, one channel per session.
- **Verification you can trust** — `--verify` proves the destination bytes
  match the source (hashed on the fly, no re-reads); `--remove-source-files`
  frees the source disk only after the copy is hash-verified, fsynced, and
  re-checked — safe for clearing an instrument's storage.
- **rsync semantics, minus the setup** — `-a`, `--delete`, `--backup`,
  `--exclude-from`/`--include-from`, itemize/stat/reporting flags, `--no-*`
  opt-outs, exit code 23. If you know rsync, you know cp2. For scripting and
  auditing, `-i/--itemize-changes` prints per-file change lines, `--stats` a
  post-run block, and `--list-only` a source listing without touching the
  destination.
- **Realtime watch** — `-W` syncs changes as they happen (event-driven push,
  server-driven pull), with a duration cap built in.
- **Cross-platform, embeddable** — any combination of Linux, macOS, and
  Windows; the sync engine runs over any byte stream.

### Why not just rsync?

rsync is battle-tested and ubiquitous — cp2 doesn't argue with that. It
argues with the rough edges:

| | rsync | cp2 |
|---|---|---|
| server setup | manual install, PATH config, or an rsync daemon | **auto-deploy on first sync** |
| delta | fixed-size blocks + rolling checksums | **content-defined chunks (FastCDC) + BLAKE3** |
| integrity | none built in | **`--verify`; hash-guarded `--remove-source-files`** |
| watch | not built in | built-in `-W` |
| password prompts | once per session (unless ssh config) | **once per run** |
| Windows | via WSL/Cygwin | **native** |

## Quick start

```bash
# Install the CLI (or grab a release tarball for all platforms)
cargo install cp2

# First sync: pushes ./photos to ~/backup on the server, deploying cp2 there
cp2 ./photos user@server:backup

# Watch the folder and sync every change for the next 24h
cp2 -W ./photos user@server:backup

# Restore: pull it back down
cp2 user@server:backup ./restore

# Local copy or sync (same engine, no ssh)
cp2 ./photos ~/backup

# A single file
cp2 peace.mp4 somewhere/else
```

Your SSH credentials are used as-is (key, agent, or password); everything on
the remote runs as *your* account.

## Common tasks

```bash
# Backup (push) — the remote path is relative to your home
cp2 ./data user@host:backup

# Custom SSH port (ports are a flag, never part of the target)
cp2 -p 2222 ./data user@host:backup

# Restore (pull) — absolute remote paths work too
cp2 user@host:/data/archive ./restore

# Preview first, then sync
cp2 -n ./data user@host:backup
cp2 ./data user@host:backup

# Sync only what matches (quote the glob — the shell would expand it)
cp2 './src/*.rs' user@host:backup

# Move data off a capture instrument, then free its disk
cp2 --verify --remove-source-files /data user@host:storage

# Keep watching a folder for 12 hours
cp2 -W=12h ./data user@host:backup

# What changed? per-file itemize lines, then a stats block
cp2 -i --stats ./data user@host:backup
#     cd+++++++++ sub                 new directory
#     .f.......... a.txt              already in sync
#     >f.s.......  a.txt              updated
#     *deleting    stale.txt          removed (with --delete)

# List the source without transferring (local sources)
cp2 --list-only ./data ./here
```

### Paths

The remote `:path` follows rsync: a leading `/` (`user@host:/abs/path`) is an
**absolute** server path; anything else (`user@host:backup`,
`user@host:softwares/cp2`) is relative to the serve root (your account home);
no path means the home itself. Traversal (`..`) is rejected.

### Glob sources

A **quoted** glob in a local source is expanded by cp2, and every match syncs
as a top-level entry of one run:

```bash
cp2 './src/*.rs' user@host:backup      # backup/a.rs, backup/b.rs
cp2 'src/**/x.rs' ./restore            # restore/a/x.rs (structure under src kept)
cp2 './*' user@host:backup --exclude target
```

Only local sources expand (remote-side expansion isn't supported); a path
that literally exists is never treated as a pattern; and the wildcard matches
dotfiles like rsync's (use `--exclude` to drop `.git`, `target`, ...).
`--watch` needs a single directory source, not a glob.

### File lists

`--files-from FILE` syncs exactly the absolute paths listed in `FILE`,
mirroring each one's root-relative structure under the destination — handy
for a curated backup manifest that reaches across directories:

```bash
cp2 --files-from manifests/microscopy.txt user@host:backup
```

```
/data/a.txt       →  backup/data/a.txt
/games/b.exe      →  backup/games/b.exe
```

One absolute path per line — both Unix and Windows line endings work, blank
lines are skipped, and a path may contain spaces or commas. On Windows the
drive letter is the root that gets dropped when mirroring:
`D:\data\a.txt` → `backup/data/a.txt` (so a list cannot mix `D:` and `E:`
entries). Relative entries are rejected (a mix of relative and absolute
would be ambiguous). Entries may be files or directories (directories
recurse); a directory and files inside it can be listed together freely —
each file syncs exactly once. Missing entries are warned about and skipped.
SRC is not used: pass only the destination. Local entries only.

### Defaults

cp2 behaves like **`rsync -avP` without `-z`**: recursive sync with
mode/mtime preservation, a per-file listing annotated with the file's
position and the run's total (`[12/3456] path`), live progress on a terminal
showing the percentage, the transfer speed, and the remaining count, and
partial files kept at the destination when a transfer is interrupted (the
next run delta-resumes against them). Compression is opt-in via `-z`; the
listing and progress are silenced by `-q`.

## Options

| Flag | rsync equivalent | Meaning |
|------|------------------|---------|
| `-p, --port PORT` | — | SSH port (default 22). Ports are never parsed from the target string — a numeric suffix like `host:2222` is a path |
| `--jump-host USER@HOST[:PORT]` | `ProxyJump` | Tunnel through a jump host (russh transport, the Windows default; system ssh reads `ProxyJump` from `~/.ssh/config` instead) |
| `--password PASSWORD` | — | Supply the target ssh password directly instead of prompting. The russh transport (Windows) sends it once and scrubs it from memory immediately; the system-ssh transport (Unix) injects it natively into the master ssh spawn on a pty (the sshpass mechanism, no external tool; password auth is forced and the master uses a run-unique socket, so the injection always sees a prompt) and scrubs it right after. Visible in the process list and shell history while the run is active — prefer keys or the prompt |
| `--jump-password PASSWORD` | — | Password for the jump host (`--jump-host`) when it differs from `--password`; without it the jump reuses `--password` (or the prompted value). Same visibility and scrubbing caveats |
| `--password-file FILE` | — | Read the password from FILE (first line) instead of `--password` — only the file path rides the command line, so the secret never appears in the shell history or process list. Mutually exclusive with `--password`; keep the file `chmod 600` |
| `--remote-path PATH` | `--rsync-path` | Path of `cp2` on the remote, used as the remote command (default `~/.cargo/bin/cp2`). Shell-quoted on Unix remotes — spaces and shell metacharacters work, `~` still expands; on Windows remotes it is interpolated into a cmd/PowerShell command, so keep it free of spaces, quotes, and metacharacters |
| `--binaries-dir DIR` | — | Directory holding prebuilt `cp2-<triple>` sidecars (checked before the directory next to this binary) |
| `--no-auto-install` | — | Don't check/deploy the server binary before syncing |
| `-a, --archive` | `-a` | Full archive (byte-identical): recursion + mode/mtime/symlinks/hard links (always on) plus special files (fifos, sockets, devices), owner/group (best-effort `chown`), SUID/SGID/Sticky, and `--literal-links` (literal link targets) — **Unix-like systems only**, silently skipped on Windows. Default (no `-a`) is 0-Root: permission bits only, owner is the SSH connection user |
| `--literal-links` | — | Keep links and shortcuts as they are (rsync `-l`): every symlink is recreated with its **literal target string** (no DEST-relative rewrite, no external-link dereference/skip), and Windows-source `.lnk` shortcuts are copied as opaque files. On a Windows target a symlink still materializes as a `.lnk`, but the target stays literal. Implied by `-a`; overridden by `--skip-links`/`--follow-links` |
| `--literal-internal-links` | — | Keep *internal* symlinks (targets resolving inside SRC) with their literal target instead of the DEST-relative rewrite; external links keep the default policy |
| `--literal-external-file-links` | — | Keep *external file-target* symlinks as links with their literal absolute target instead of dereferencing them (high risk: the destination must have the same absolute path). Ignored when the destination is Windows |
| `--literal-external-dir-links` | — | Keep *external directory-target* symlinks as links with their literal target instead of skipping them (high risk: the destination must have the same absolute path). Dangling external links are still skipped unless `--literal-links` |
| `--follow-links` | `-L` | Dereference every symlink (rsync `-L`): the target's content is copied in the link's place — file targets as regular files, directory targets recursed (with loop detection), Windows-source `.lnk` shortcuts followed to their target. Overrides the `--literal-*` family |
| `--skip-links` | `--no-l` | Skip every symlink and shortcut entirely: not synced, not followed (rsync `--no-l`). Highest priority — overrides all other link flags |
| `--no-perms` | `--no-p` | Don't preserve permission bits: the destination gets explicit 0644/0755 defaults (files/dirs) and the Windows-source executable heuristic is disabled |
| `-S, --sparse` | `-S` | Write files sparsely on the receiver: runs of zeros ≥ 4096 bytes become holes instead of allocated blocks (VM images, database files). Content bytes are unchanged |
| `-X, --xattrs` | `-X` | Copy extended attributes for files and directories (best-effort: unsettable names warn and skip; symlinks not covered). On Linux, POSIX ACLs ride along as `system.posix_acl_*` attributes |
| `-U, --atimes` | `-U` | Restore the source's last-access time; without it the receiver's atime is left alone. Independent of `--no-times`; never part of the quick check |
| `--no-times` | `--no-t` | Don't preserve mtimes (destination gets transfer time; quick check becomes size-only) |
| `--no-recursive` | `--no-r` | Sync only the source root's direct files; skip subdirectories |
| `-n, --dry-run` | `-n` | Print the transfer direction and exit without connecting (a local preview — rsync's `-n` runs a full remote dry scan with a per-file change list; cp2's `-n` is intentionally lighter) |
| `-W, --watch[=DUR]` | — | Watch SRC and sync changes continuously (push/local: event-driven; pull: server-driven); optional d/h/m/s duration (`-W=1h30m`), default 24h |
| `--watch-delay MS` | — | Debounce quiet window for `--watch` (default 1000) |
| `--delete` | `--delete` | Remove destination files not present in source |
| `--max-delete N` | `--max-delete` | Refuse to delete more than N files with `--delete` |
| `-u, --update` | `-u` | Skip files where destination is newer |
| `-c, --checksum` | `-c` | Compare BLAKE3 hashes instead of size+mtime |
| `--ignore-existing` | `--ignore-existing` | Skip files that already exist |
| `--existing` | `--existing` | Only update existing files; don't create new ones |
| `--ignore-times` | `-I` | Transfer all files, ignoring the size+mtime quick check |
| `--backup` | `--backup` | Keep the replaced destination as `<name>~` |
| `--remove-source-files` | `--remove-source-files` | Delete source files only after the destination is **hash-verified and fsynced** (BLAKE3 compare, per-file fsync, size+mtime re-checks both sides; move-off workflows; directories and symlinks are kept) |
| `--verify` | — | Verify the destination bytes match the source after the transfer (BLAKE3 on the fly, no re-reads; mismatches → exit 23, nothing deleted) |
| `--files-from FILE` | `--files-from` | Sync only the listed **absolute** paths (one per line, Unix or Windows line endings), mirrored under DST; relative entries rejected; SRC not used (`cp2 --files-from FILE DST`). On a remote pull the entries are server-absolute paths |
| `--exclude GLOB` | `--exclude` | Exclude matching paths (repeatable) |
| `--include GLOB` | `--include` | Include matching paths (repeatable) |
| `--exclude-from FILE` | `--exclude-from` | Read additional exclude patterns from FILE (one per line; blank lines and `#`/`;` comments ignored) |
| `--include-from FILE` | `--include-from` | Read additional include patterns from FILE (same format) |
| `-i, --itemize-changes` | `-i` | Print a per-file change line: `>f`/`cd` new, `>f.s` updated, `*deleting` removed, `.f` in-sync (rsync `-i`; covers push/local out of the plan, pull from the reproduced plan). A chmod'ed in-sync file is re-applied as an attribute-only update and shows as `>f.s` with zero bytes |
| `--stats` | `--stats` | Print a detailed statistics block (files, bytes, time, skipped) after the summary |
| `--list-only` | `--list-only` | List the source files without transferring (local and remote sources) |
| `-j, --jobs N` | — | Parallel transfer + hash workers; omitted = auto-tuned from the target storage class |
| `--storage auto\|hdd\|ssd` | — | Storage class for auto-tuning: detect on Linux/Windows/macOS (default), or force HDD/SSD |
| `-z, --compress` | `-z` | Compress the data stream (lz4) |
| `--bwlimit SIZE` | `--bwlimit` | Limit transfer bandwidth (e.g. 10M, 100K) |
| `--fsync` | — | fsync every received file before it is renamed into place (durable but slower; off by default) |
| `-v, --verbose` | `-v` | Verbosity (repeat for more: -v, -vv, -vvv) |
| `-q, --quiet` | `-q` | Suppress the non-error output: no per-file listing, no transfer summary, no deploy/watch lines (errors and the skipped-file report still print) |

`cp2 --server` is the sshd-invoked server mode (like rsync's `--server`): it
reads the protocol from stdin and writes to stdout. Not for direct use.

## How it works

A sync deploys the server binary if needed, exchanges file manifests, plans
create/update/delete, and sends only the changed bytes — FastCDC
content-defined chunks plus BLAKE3, so a 1-byte edit near the start of a 50 GB
file transfers kilobytes, not gigabytes. The full pipeline, the module map,
the deployment mechanics, the security model, and the benchmark suite live in
[`ARCHITECTURE.md`](ARCHITECTURE.md); building and testing are documented
there too.

The client machine needs the `ssh` client (default on Linux, macOS, and
modern Windows). On the first sync the remote `cp2 --server` is
**auto-deployed** — disable with `--no-auto-install`, or prebuilt
`cp2-<triple>` sidecars (e.g. `cp2-x86_64-unknown-linux-musl`) can be dropped
in `--binaries-dir` or next to the client; the [GitHub releases
page](https://github.com/elemeng/cp2/releases) carries prebuilt binaries for
all platforms.

## Performance comparison

Measured on a native Fedora 44 (x86_64, NVMe) machine, pushing over
`ssh localhost` (1 MiB pipes on both ends; page cache warmed before each
tool), from `bench/compare_test.sh` — cp2 vs rsync vs scp vs sy:

```
tool      large-first   large-edit  small-first   small-idle
cp2             2.55s        2.54s        1.78s        0.65s
rsync           2.39s        1.51s        2.03s        0.80s
scp             2.26s        2.26s        3.98s        4.01s
sy              2.59s        4.73s       14.29s        0.89s
```

(The cp2 rows predate the delta-overlap optimization described under the
mixed-tree table: after it, cp2's large-edit re-measures at ≈1.8s vs
rsync's ≈1.6s — see below.)

### Mixed tree (≈10 GiB, 100 K files)

`bench/mixed-tree.sh` — 70 K small 1-16 KiB, 27 K medium 64-384 KiB, 3 K
large 1-2 MiB files, phases over unchanged sources (2026-08-26, same host;
cp2 used the russh transport):

| Phase | cp2 | rsync |
|-------|-----|-------|
| fresh (11.4 GB) | 47.96s | 51.56s |
| second (no-op quick check) | 2.18s | 1.48s |
| edit (1 K appends + 0.8 K rewrites + 200 new + 100 deleted) | 54.82s | 10.21s |

Both destinations end byte-identical to the edited source (`rsync -rltc`
dry-run: 0 differing files on each side). cp2 wins the 100 K-file fresh
push; the edit phase favored rsync on localhost — cp2 sent only ~22 MB of
changed bytes but the basis signature and the delta compute serialized
across 100 K files. That serialization is gone since this run: the sender's
source chunking now runs concurrently with the receiver's basis signing
(the two full-file passes execute on different machines), which re-measures
the single-file large-edit from 2.53s to ≈1.8s. (This run's first attempt
used the system-ssh transport and deadlocked in OpenSSH's ControlMaster mux
on a server-side stderr write — 25 min with no progress; the same phase
over the russh transport completed in 55s. The system-ssh path is under
investigation.)

### The studied crates, honestly

The sibling crates cp2's design was informed by were put through the same
harness (`bench/compare_studied.sh`, same host and same run, 2026-08-26).
The participation audit comes first — of the eight crates, **three can sync
over ssh at all**:

| Tool | Verdict |
|------|---------|
| ripsync | syncs over ssh, but its protocol rejects frames ≥ 512 MiB — **fails (rc=1) on any single file that large**; only the small-tree rows below are valid |
| pxs | syncs over ssh (integrity-first), no limits found |
| sy | syncs over ssh; **msy ships the same `sy` binary** (plus `sy-scan`/`sy-remote`/`sy-bench-gen`) — the two are the same tool, already on the list |
| syncz | goes over ssh but is a **wrapper around the system rsync** — benchmarking it is benchmarking rsync again |
| sparsync | no drop-in ssh sync — needs a `serve`/`enroll`/auth mesh |
| zsync-rs | HTTP delta client — no ssh at all |
| robosync, rusync | do **not parse** `user@host:path` at all — they copy into a literal local directory named `user@host:path` under the working directory (rc=0, "9 bytes transferred", no ssh connection — verified in sshd's journal) |
| copia | library, no CLI |

The four-scenario results (1 GiB fresh/edit + 8192-file tree; destination
bytes verified against each tool's edited source for the successful rows;
per-tool runs bounded by a 300s timeout, rc recorded; re-run 2026-08-26
after the delta-overlap work, which is why cp2's large-edit dropped from
the earlier 2.53s):

```
tool      large-first   large-edit  small-first   small-idle
cp2             1.94s        1.78s        1.85s        0.64s
rsync           2.08s        1.46s        1.67s        0.85s
sy              2.42s        4.42s       12.21s        0.72s
pxs             6.40s        0.97s        2.90s        1.54s
ripsync         1.34s        1.36s        3.27s        0.71s  rc!=0: large-first, large-edit
```

Reading them honestly: cp2 leads the idle re-sync (0.64s vs rsync 0.85s)
and stays within ~11% of rsync on the small-file first sync; the large-edit
gap to rsync's rolling-checksum delta is down to ~22% (cp2's source
chunking overlaps the basis signing). pxs has the fastest delta edit
(0.97s, its integrity hashing makes it the slowest fresh transfer at 6.40s
— 3.3x cp2); sy trails everywhere, ~6.6x slower than cp2 on many small
files; ripsync cannot handle a >512 MiB file at all and is ~1.8x slower
than cp2 on the small tree. Localhost runs vary ~±30% between runs even
for the same tool — compare tools within a run, not across runs.

The full benchmark suite — scripts, generated trees, and how to run it — is
documented in [`ARCHITECTURE.md`](ARCHITECTURE.md).

## Contributing

Issues and pull requests are welcome — open an
[issue](https://github.com/elemeng/cp2/issues) or a
[pull request](https://github.com/elemeng/cp2/pulls). A few ground rules:

- **Bug reports**: include the exact command, `cp2 --version` output, and the
  platforms involved (local and remote).
- **Before opening a PR**: `cargo test` must pass and
  `cargo clippy --all-targets -- -D warnings` must be clean.
- **Be honest about provenance**: the design adapts several MIT/BSD projects
  (attributed in the README table and [NOTICE](NOTICE)) — credit adaptations
  in the module headers, and don't import code from GPL projects.
- **Larger features** get discussed in an [issue](https://github.com/elemeng/cp2/issues) first.

## Acknowledgments

cp2's design, and parts of its code, are informed by the following projects.
None are linked at runtime — adaptations are credited per module in the
source headers, and their retained copyright lines live in [`NOTICE`](NOTICE).

| Project | License | Contribution |
|---|---|---|
| rsync | GPL-3.0 | CLI semantics (`-a`, `--delete`, the `--no-*` opt-outs), the rsync-over-ssh model — design only, no code |
| copia | MIT | Delta engine: `Delta`/`DeltaOp`/`apply_patch` (adapted, `src/delta/`) |
| sparsync | MIT | Frame wire protocol; scan/manifest pipeline (adapted) |
| sy | MIT | Planner; transport abstraction (adapted) |
| robosync | MIT | File-tier transfer strategy (adapted, `src/sync/strategy.rs`) |
| pxs | BSD-3-Clause | Staged-file sink / atomic commit (adapted, `src/platform/staging.rs`) |
| librsync | MIT/Apache-2.0 | Delta-algorithm background (studied) |
| rusync | BSD-3-Clause | rsync-style CLI/protocol study |
| zsync-rs | MIT | rsync-compatible delta study |
| ripsync | MIT OR Apache-2.0 | rsync-compatible sync study |
| msy | MIT | Sync-pipeline study |
| syncz | MIT | Sync-protocol study |

All adaptations are from MIT/BSD-licensed projects, compatible with cp2's
MIT license; rsync (GPL) contributed design and interface semantics only.

## License

MIT — see [LICENSE](LICENSE). Third-party adaptations and their retained copyright lines are attributed in [NOTICE](NOTICE).
