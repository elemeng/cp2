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

## How the delta engine works

Files are split into chunks at **content-defined boundaries** (FastCDC) —
boundaries computed from the bytes themselves, so they don't move when you
edit. Each chunk is identified by a BLAKE3 hash.

When syncing, cp2 asks the destination for the chunk signature of the old
file, then sends only the chunks whose hash isn't there. Because boundaries
are content-defined, a 1-byte insertion near the start of a 50 GB file
shifts nothing — only the chunk or two around the edit crosses the wire.
The receiver reconstructs the file and verifies it against the delta's
checksum.

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
| `--files-from FILE` | `--files-from` | Sync only the listed **absolute** paths (one per line, Unix or Windows line endings), mirrored under DST; relative entries rejected; SRC not used (`cp2 --files-from FILE DST`) |
| `--exclude GLOB` | `--exclude` | Exclude matching paths (repeatable) |
| `--include GLOB` | `--include` | Include matching paths (repeatable) |
| `--exclude-from FILE` | `--exclude-from` | Read additional exclude patterns from FILE (one per line; blank lines and `#`/`;` comments ignored) |
| `--include-from FILE` | `--include-from` | Read additional include patterns from FILE (same format) |
| `-i, --itemize-changes` | `-i` | Print a per-file change line: `>f`/`cd` new, `>f.s` updated, `*deleting` removed, `.f` in-sync (rsync `-i`; covers push/local out of the plan, pull from the reproduced plan) |
| `--stats` | `--stats` | Print a detailed statistics block (files, bytes, time, skipped) after the summary |
| `--list-only` | `--list-only` | List the source files without transferring (local sources; remote needs the manifest round-trip and is not yet wired) |
| `-j, --jobs N` | — | Parallel transfer + hash workers; omitted = auto-tuned from the target storage class |
| `--storage auto\|hdd\|ssd` | — | Storage class for auto-tuning: detect on Linux/Windows/macOS (default), or force HDD/SSD |
| `-z, --compress` | `-z` | Compress the data stream (lz4) |
| `--bwlimit SIZE` | `--bwlimit` | Limit transfer bandwidth (e.g. 10M, 100K) |
| `--fsync` | — | fsync every received file before it is renamed into place (durable but slower; off by default) |
| `-v, --verbose` | `-v` | Verbosity (repeat for more: -v, -vv, -vvv) |
| `-q, --quiet` | `-q` | Suppress the non-error output: no per-file listing, no transfer summary, no deploy/watch lines (errors and the skipped-file report still print) |

`cp2 --server` is the sshd-invoked server mode (like rsync's `--server`): it
reads the protocol from stdin and writes to stdout. Not for direct use.

## How the server works

By default every sync **deploys itself**: the platform is read from the
sync session's in-band preamble and the remote binary's freshness is
verified by the Hello handshake on that same session. When the binary is
missing or stale, the client streams a matching build to the remote and
`exec`s it as the server **on the same session** — the deploy session is the
sync session, the Hello verifies the deploy, and the whole stale/missing
case costs two ssh sessions (the failed attempt + the deploy-and-serve). The
first `cp2 ./data user@host/backup` against a fresh account just works.
Disable with `--no-auto-install` (e.g. for a managed server install).

**Platform portability:** the deployed binary must match the server. The
client prefers a prebuilt **sidecar** named `cp2-<triple>` (e.g.
`cp2-x86_64-unknown-linux-musl` for a Linux server) — a Linux sidecar is a
statically linked musl build that runs on any remote glibc — found in
`--binaries-dir` or next to the client binary. Without a sidecar, a
same-platform remote gets the running binary (which needs the local glibc —
a remote with an older one fails at load time, GLIBC_2.xx not found). The
platform is detected from the session's preamble (`uname -s -m`, falling
back to `cmd /c echo %PROCESSOR_ARCHITECTURE%` on Windows). Nothing is
downloaded at sync time — if a cross-platform sidecar is missing, cp2 tells
you to fetch it from the [GitHub releases page](https://github.com/elemeng/cp2/releases)
and drop it in one of those two places (or pass `--no-auto-install`).

Build Linux sidecars with a static libc (the default glibc build only runs on
glibc systems):

```bash
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
# copy target/<triple>/release/cp2 → cp2-<triple> (next to the client or in --binaries-dir)
```

Supported triples: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-gnu`,
`aarch64-pc-windows-gnu`. Windows clients bundle the pure-Rust russh
transport, whose aws-lc-rs crypto backend is C-based: cross-building the
Windows sidecars from Linux needs a mingw-w64 C compiler for the target
(`gcc-mingw-w64-x86-64`, and `gcc-mingw-w64` on newer distros for aarch64).
NASM is never required: cp2's manifest enables aws-lc-sys's `prebuilt-nasm`
feature, so the crate's shipped prebuilt objects are used whenever NASM is
not on PATH (native `cargo install cp2` on Windows needs only the MSVC C
toolchain; a found NASM is used if present, never demanded). Windows
sidecars ship as `cp2-<triple>.exe` and are found under either name when
auto-deploying.

**Windows remotes** get a PowerShell + `certutil` deploy (base64 over stdin —
Windows' 32 KB command-line limit rules out inline base64) and a
`cmd /c`-wrapped server command so `%USERPROFILE%` paths expand under any
sshd default shell; the default remote path is
`%USERPROFILE%\.local\bin\cp2.exe`. The platform probe is instant and
locale-independent.

**Authentication** is your SSH setup, untouched: keys, agent, or password —
whatever sshd accepts (PAM on Linux/macOS, LogonUser/keys on Windows
OpenSSH). Access is scoped exactly like SSH: you reach what your account can
reach, and the remote `cp2 --server` runs as you.

**Transport** — on Unix the system `ssh` process carries the protocol
(rsync's model): all of cp2's ssh sessions (platform probe, deploy, sync)
multiplex over one master connection, so with password auth you type your
password **once per run** — and not again for a later run within a minute.
On Windows, where OpenSSH's `ControlMaster` socket is unusable
(`getsockname failed: Not a socket`), cp2 uses its own pure-Rust SSH client
(russh): one connection, one authentication, and one channel per session
(no multiplexing machinery at all). The russh transport covers keys
(including encrypted keys and OpenSSH user certificates), the SSH agent
(Windows named pipe / Pageant), keyboard-interactive, and password; host
keys follow OpenSSH semantics (`~/.ssh/known_hosts` with trust-on-first-use
and `@cert-authority` host-certificate verification). GSSAPI and FIDO
security keys are system-ssh-only. `--jump-host user@host[:port]` tunnels
through a jump host on the russh transport (OpenSSH `ProxyJump` semantics);
system ssh reads `ProxyJump` from `~/.ssh/config` instead.

## How it works

The sync pipeline separates pure decision logic from async I/O orchestration:

```
scanner → Manifest → planner → SyncPlan → strategy → executor → verify
```

| Module | Role |
|--------|------|
| `delta/` | Pure FastCDC engine: content-defined chunks, hash-index signature, `compute_delta`, `apply_patch` |
| `sync/scanner` | Walk a directory into a serializable `Manifest` (streaming hashes, include/exclude filters) |
| `sync/filter` | Pure rsync-style include/exclude glob matching |
| `sync/planner` | Pure: manifest × manifest → `SyncPlan` (create/update/delete/skip) |
| `sync/strategy` | Pure: file size → transfer strategy (copy/delta) |
| `sync/sender` | Async sender role: manifest exchange, batching, delta recipes |
| `sync/receiver` | Async receiver role: staged atomic apply, on-demand signatures |
| `sync/executor` | Orchestrates push/pull/serve over one byte stream (the ssh channel) |
| `transport/ssh` | Spawns `ssh -p PORT user@host cp2 --server` (rsync's model) |
| `platform/staging` | `StagedFile`: seeded, preallocated temp file + atomic commit |
| `protocol/` | Frame wire format with a version-only Hello handshake |

A deeper walkthrough — the full push/pull session diagram, the delta engine,
and the receiver's atomic-apply pipeline — lives in
[`ARCHITECTURE.md`](ARCHITECTURE.md).

## Performance comparison

`bench/compare_test.sh` pushes the same trees over ssh with cp2, rsync, scp,
and [sy](https://crates.io/crates/sy), across four scenarios:

| Scenario | Source | What it measures |
|----------|--------|------------------|
| large-first | 1 GiB (2 × 512 MiB, generated) | raw transfer throughput, fresh destination |
| large-edit | same tree, 1 MiB overwritten mid-file | delta/incremental behavior on a changed large file |
| small-first | 8192 files, 1-64 KiB, 64 dirs (generated; `--small-src` to point at a real tree) | per-file overhead, fresh destination |
| small-idle | unchanged tree | quick-check / scan overhead of a no-op sync |

Measured on a WSL2 (Fedora 42) machine, pushing over `ssh localhost`
(1 MiB pipes on both ends; page cache warmed before each tool):

```
tool      large-first   large-edit  small-first   small-idle
cp2             3.46s        2.90s        5.45s        1.94s
rsync           3.48s        2.60s        1.55s        0.67s
scp             2.71s        2.04s       14.20s       11.34s
sy              5.74s       13.36s       54.56s        0.67s
```

(2026-08 re-run after the pipeline work: phase-split CDC scan, parallel
batch hashing, single-pass apply hashing, opt-in whole-file checksum. This
run's host was ~25% slower overall than the 2026-07 baseline — scp's raw
large-first copy went 2.18s → 2.71s — so compare scenarios, not raw
seconds. The delta-path work shows up in **large-edit**: cp2 3.38s → 2.90s
on a slower machine, closing the gap to rsync from ~2x to ~1.1x.)

Reading the numbers honestly:

- **Large first sync:** cp2 ≈ rsync, within noise of scp. All three are pinned
  near the machine's ssh-layer ceiling (~715 MB/s on this WSL2 host).
- **Large edit:** rsync's rolling-checksum delta still wins on localhost
  (cp2's CDC delta re-reads the basis twice — signature + compute), but the
  gap narrowed to ~10% since the whole-file checksum became opt-in (the
  default path no longer runs a second full-file hash on either side). On a
  real 1 GbE/10 GbE link the delta's wire savings (1 MiB vs 1 GiB) flip this
  in cp2's favor — the same reason rsync beats scp on real networks.
- **Many small files:** rsync is fastest (no per-file atomic staging, no
  manifest exchange); cp2 pays for its guarantees — every file lands via a
  staged temp + atomic rename, and every sync exchanges manifests — and still
  beats scp ~2.6x on the first sync and ~6x on the idle re-sync (scp has no
  quick check and re-copies everything).
- **sy 0.4.0** trails on every scenario except the idle scan.

Run it yourself: `bench/compare_test.sh` (needs ssh key auth to the target;
`--remote user@host`, `--small-src DIR`, `--large-mb N` to adjust).

## Build & roadmap

```bash
cargo build
cargo test
cargo clippy
```

Requires the `ssh` client on the machine running cp2 (installed by default on
Linux, macOS, and modern Windows). Optionally pin the server as an sshd forced
command in `~/.ssh/authorized_keys`:
`command="cp2 --server",restrict ssh-ed25519 AAAA...`

The **distribution tarball** (all platforms + `install.sh`) is built by the
GitHub Actions workflow on a `v*` tag; `scripts/build-release.sh` does the
same locally. See `scripts/` and `.github/workflows/release.yml`.

Planned work lives in [`ROADMAP.md`](ROADMAP.md): v0.2 content-addressed
dedup (`--dedup-host-ref`), v0.3 bidirectional sync.

## Changelog

### v0.1.1

- **Windows install fix**: `cargo install cp2` no longer requires NASM — the
  manifest enables aws-lc-sys's `prebuilt-nasm` feature, so its shipped
  prebuilt objects are used whenever NASM is not on PATH (a Windows machine
  with just the MSVC C toolchain builds fine).
- **Data-loss fix on pull**: `--verify` no longer implies
  `--remove-source-files`. The two flags were coupled in the remote server
  invocation, so a *pull* with only `--verify` deleted every verified file on
  the remote; a verified-only pull now deletes nothing.
- **Windows build fix**: the `--password` path compiled an undeclared
  identifier in a Windows-only branch; the crate builds cleanly on Windows
  again.
- **SSH transport hardening** (system ssh and the pure-Rust russh client):
  - `--password` against a host with an unknown host key no longer hangs
    forever — host-key prompts are answered `no` (fail-closed; keys are never
    auto-accepted), and every ssh sub-step (probe, version check, deploy,
    password-prompted spawn) is bounded by a timeout.
  - `--password`/`--jump-password` are scrubbed from memory on **every** exit
    path, including connection failures (previously the value could linger in
    heap memory when a connect attempt failed early).
  - Authentication/network failures during the platform probe now fail the
    run with a clear error instead of being masked as "unknown platform" and
    surfacing later as a confusing deploy failure.
  - `--remote-path` is shell-quoted on Unix remotes (spaces and shell
    metacharacters are safe, `~` still expands); deploy is atomic (stream to
    a temp file, then rename), so an interrupted deploy never leaves a
    truncated binary at the destination.
  - The russh client follows OpenSSH `known_hosts` semantics: rotated or
    duplicate keys are accepted when **any** entry matches, malformed lines
    are skipped with a warning, `@revoked` entries are honored, host
    certificates must pass critical-option checks, and hosts match by name
    and resolved address.
- **Parsing fixes**: `--jump-host` accepts bracketed IPv6 (`user@[::1]:2222`)
  and rejects malformed ports; remote targets parse rsync-style first-colon
  paths (`host:a:b`), treat `@`-containing local paths as local, and reject
  empty user/host up front.
- **Watch fix**: Ctrl-C during the initial-sync retry backoff stops the
  session cleanly instead of starting the watcher.
- **Local copy fix**: `-j`, `--max-delete`, and `--storage` are passed to the
  local `cp2 --server` child as proper arguments again.

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
- **Larger features** get discussed in [ROADMAP.md](ROADMAP.md) first.

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

MIT — see [LICENSE](LICENSE). Third-party adaptations and their retained
copyright lines are attributed in [NOTICE](NOTICE).
