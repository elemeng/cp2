# cp2

[![Crates.io](https://img.shields.io/crates/v/cp2.svg)](https://crates.io/crates/cp2)
[![docs.rs](https://docs.rs/cp2/badge.svg)](https://docs.rs/cp2)
[![License](https://img.shields.io/crates/l/cp2.svg)](https://crates.io/crates/cp2)

**Copy and sync files locally or between machines — ultra fast, verified,
private, and with zero server setup.**

cp2 is a modern `cp`/`rsync`-style tool in one pure-Rust binary for **Linux**,
**macOS**, and **Windows**. It sends only the bytes that actually changed,
verifies what it writes, and watches folders in realtime; a same-platform
remote needs nothing installed — the first sync deploys cp2 there
automatically.

- **Install:** one binary, or `cargo install cp2`
- **First sync:** `cp2 ./photos user@server:backup`
- **Watch mode (sync in realtime):** `cp2 -W ./photos user@server:backup`

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Why cp2?](#why-cp2)
- [Common tasks](#common-tasks)
- [Paths, globs, and file lists](#paths-globs-and-file-lists)
- [Defaults](#defaults)
- [Key options](#key-options)
- [Performance](#performance)
- [How it works](#how-it-works)
- [Contributing](#contributing)
- [Acknowledgments](#acknowledgments)
- [License](#license)

## Install

### On your machine

The recommended way — just note it needs the Rust toolchain on your PC:

```bash
cargo install cp2
```

or grab a prebuilt binary for your platform from the [GitHub releases
page](https://github.com/elemeng/cp2/releases) — download, extract, run.
There is nothing else to install: on Unix cp2 rides the `ssh` client you
already have, and on Windows a pure-Rust SSH client is built in.

### On a remote (once, for different platforms)

A **same-platform** remote needs nothing — cp2 deploys itself there on the
first sync. A **different platform** needs cp2 installed on the remote
once, any of these three ways:

1. **Install with cargo** (Recommend) — log in and run `cargo install cp2 --locked`
   (it builds for the remote's own libc, so it also fixes an older-glibc
   remote):

   ```bash
   ssh user@remote
   cargo install cp2 --locked #Recommend
   ```

2. **Download the prebuilt tarball** — grab the release tarball for the
   remote's platform from the [GitHub releases
   page](https://github.com/elemeng/cp2/releases), extract it, rename the
   binary to `cp2`, and copy it to `~/.cargo/bin/`.

3. **Build from source** — on the remote:

   ```bash
   git clone https://github.com/elemeng/cp2 && cd cp2
   cargo build --release
   cp target/release/cp2 ~/.cargo/bin/
   ```

The binary is plain `cp2` on Linux **and** macOS — only Windows appends
`.exe` — and the install folder is `~/.cargo/bin` on Unix,
`%USERPROFILE%\.cargo\bin` on Windows (`cp2.exe`). That is also exactly
where `cargo install` puts it and where the client probes by default, so
any of the three methods lands the binary in the right place.

The remote build must match your client's build — the sync handshake
enforces it, and the error names what to reinstall. After that,
`cp2 ./data user@remote:backup` syncs like any same-platform remote.

## Quick start

```bash
# Your first sync: push a folder to a server (same-platform remotes auto-deploy)
cp2 ./photos user@server:backup

# Local copy — the same engine, no ssh
cp2 ./photos ~/backup

# Restore: pull it back down
cp2 user@server:backup ./restore

# Watch the folder and sync every change for the next 24 hours
cp2 -W ./photos user@server:backup
```

Your existing SSH credentials are used as-is (key, agent, or password);
everything on the remote runs as *your* account.

## Why cp2?

- **Sends only what changed** — FastCDC chunking + BLAKE3: a one-byte edit in
  a 50 GB file transfers kilobytes, not gigabytes, and large files stream in
  bounded memory — size never becomes a RAM problem.
- **Verification you can trust** — `--verify` proves the destination bytes
  match the source; `--remove-source-files` frees the source disk only after
  the copy is hash-verified and fsynced.
- **rsync semantics, minus the setup** — `-a`, `--delete`, `--backup`,
  excludes, `--no-*` opt-outs, exit code 23. If you know rsync, you already
  know cp2.
- **Realtime watch** — `-W` monitors a folder and syncs changes as they
  happen (event-driven push, server-driven pull), with an optional duration
  cap.
- **Cross-platform and scriptable** — any combination of Linux, macOS, and
  Windows; `-i`/`--stats`/`--list-only` give clean, parseable output.

### Why not just rsync?

rsync is battle-tested and ubiquitous — cp2 doesn't argue with that, just
the rough edges:

| | rsync | cp2 |
|---|---|---|
| server setup | manual install, PATH config, or a daemon | auto-deploy on same-platform remotes |
| delta | fixed blocks + rolling checksums | content-defined chunks (FastCDC) + BLAKE3 |
| integrity | none built in | `--verify`; hash-guarded `--remove-source-files` |
| watch | not built in | built-in `-W` |
| password prompts | once per session | once per run |
| Windows | via WSL/Cygwin | native |

## Common tasks

### Backup and restore

```bash
cp2 ./data user@host:backup           # push; remote paths are relative to your home
cp2 user@host:/data/archive ./restore # pull; absolute remote paths work too
cp2 -p 2222 ./data user@host:backup   # custom SSH port (a flag, never in the target)
```

### Preview before you sync

```bash
cp2 -n ./data user@host:backup   # list what would be transferred, change nothing
cp2 ./data user@host:backup      # then sync for real
```

### Safely move data off a machine

```bash
cp2 --verify --remove-source-files /data user@host:storage
```

Source files are deleted only after the destination is hash-verified and
fsynced — safe for clearing an instrument's storage.

### Sync only what matches

```bash
cp2 './src/*.rs' user@host:backup        # quote the glob — the shell would expand it
cp2 ./project user@host:backup \
    --exclude target --exclude .git
```

### Watch a folder continuously

```bash
cp2 -W=12h ./data user@host:backup   # the duration is optional; default 24h
```

### Scripting and auditing

```bash
cp2 -i --stats ./data user@host:backup
#     cd+++++++++ sub                new directory
#     .f.......... a.txt             already in sync
#     >f.s.......  a.txt             updated
#     *deleting    stale.txt         removed (with --delete)

cp2 --list-only ./data ./here        # list the source without transferring
```

### Jump hosts and passwords

```bash
cp2 --jump-host user@gateway ./data target@host:backup
cp2 --password-file ~/.ssh/cp2.pw ./data user@host:backup  # secret never on the command line
```

## Paths, globs, and file lists

### Remote paths

A leading `/` after the colon (`user@host:/abs/path`) is an **absolute**
server path; anything else (`user@host:backup`, `user@host:softwares/cp2`)
is relative to the serve root (your account home); no path means the home
itself. Path traversal (`..`) is rejected.

### Glob sources

A **quoted** glob in a local source is expanded by cp2, and every match
syncs as a top-level entry of one run:

```bash
cp2 './src/*.rs' user@host:backup      # backup/a.rs, backup/b.rs
cp2 'src/**/x.rs' ./restore            # restore/a/x.rs (structure under src kept)
cp2 './*' user@host:backup --exclude target
```

Only local sources expand (remote-side expansion isn't supported); a path
that literally exists is never treated as a pattern; and the wildcard
matches dotfiles like rsync's (use `--exclude` to drop `.git`, `target`,
...). `--watch` needs a single directory source, not a glob.

### File lists

`--files-from FILE` syncs exactly the absolute paths listed in `FILE`,
mirroring each one's root-relative structure under the destination — a
curated backup manifest that reaches across directories:

```bash
cp2 --files-from manifests/microscopy.txt user@host:backup

# /data/a.txt   →  backup/data/a.txt
# /games/b.exe  →  backup/games/b.exe
```

One absolute path per line — Unix and Windows line endings both work, blank
lines are skipped, and paths may contain spaces. Entries may be files or
directories (directories recurse), and each file syncs exactly once; missing
entries are warned about and skipped. Local entries only.

## Defaults

cp2's defaults mirror **`rsync -avP` without `-z`**: recursive sync with
mode/mtime preservation, a per-file listing annotated with the file's
position and the run's total, a dnf-style summary row per file
(`[12/3456] path 100% | rate | size | elapsed`), live progress on a
terminal (one in-place row per file, growing `0%`→`100%`), and partial
files kept at the destination when a transfer is interrupted — the next
run delta-resumes against them. Compression is opt-in via `-z`; the
listing and progress are silenced by `-q`.

## Key options

The full reference — every flag with its rsync equivalent and caveats — is
in the man page (`man ./docs/cp2.1`) and in `cp2 --help`. The flags people
reach for:

### Essentials

| Flag | What it does |
|---|---|
| `-a, --archive` | Byte-identical archive: recursion, mode/mtime, symlinks and hard links, plus special files, owner/group, SUID/SGID/Sticky (Unix-like systems) |
| `-n, --dry-run` | Preview what would transfer; touches nothing |
| `--delete` / `--max-delete N` | Remove destination files absent from source / refuse to delete more than N |
| `-u, --update` | Skip files where the destination is newer |
| `-c, --checksum` | Compare BLAKE3 hashes instead of size+mtime |
| `--ignore-existing` / `--existing` | Skip files that already exist / update only existing files |
| `-p, --port`, `--jump-host`, `--password`, `--password-file` | SSH connection: custom port, tunneling, credentials |

### Safety

| Flag | What it does |
|---|---|
| `--verify` | Prove the destination bytes match the source (exit 23 on mismatch, nothing deleted) |
| `--remove-source-files` | Delete source files only after the copy is hash-verified and fsynced |
| `--fsync` | fsync every received file before it is renamed into place, and the destination directories holding the renames once at the end |
| `--backup` | Keep replaced destination files as `<name>~` |

### Speed and bandwidth

| Flag | What it does |
|---|---|
| `-j, --jobs N` | Parallel transfer + hash workers (auto-tuned from the target storage class) |
| `-z, --compress` | Compress the stream (lz4) |
| `--bwlimit SIZE` | Cap transfer bandwidth (e.g. `10M`, `100K`) |
| `-S, --sparse` | Write zero runs as holes (VM images, database files) |

### Selection and scripting

| Flag | What it does |
|---|---|
| `--exclude` / `--include`, `--exclude-from` / `--include-from` | Filter what syncs (repeatable; files: one pattern per line) |
| `--files-from FILE` | Sync exactly the listed paths, mirrored under the destination |
| `-i, --itemize-changes` | Per-file change lines for scripting and auditing |
| `--stats` | Post-run statistics block |
| `--list-only` | List the source without transferring |
| `-W[=DUR]`, `--watch-delay MS` | Watch continuously; debounce window |
| `-X, --xattrs`, `-U, --atimes` | Extended attributes / access times |
| `--literal-links`, `--follow-links`, `--skip-links` | Link policy: keep, follow, or skip symlinks and shortcuts |
| `--no-recursive`, `--no-perms`, `--no-times` | Opt out of the `rlpt` core piece by piece |
| `-v, --verbose`, `-q, --quiet` | Output level (repeat `-v` for more) |

`cp2 --server` is the sshd-invoked server mode (like rsync's `--server`) —
it runs on the remote side and is not for direct use.

## Performance

One benchmark run pushing over `ssh localhost` (Fedora 44, x86_64, NVMe,
2026-08-27). Every cell is three repetitions reported as **mean ± sd**,
with the page cache warmed before each run, the tool order rotated per
scenario, every run bounded by a 300 s timeout, and both destinations
byte-verified against each tool's edited source (0 differing files). Every
tool runs its default core — cp2 bare, rsync plain `-a`, scp `-r`, sy/pxs
defaults — no `-c`/checksum flag anywhere. From `bench/bench.sh compare`.

```
tool      large-first       large-edit      small-first       small-idle
cp2       1.76±0.09s        2.05±0.53s      3.16±0.31s        0.59±0.00s
rsync     2.00±0.03s        1.97±0.71s      2.01±0.24s        0.73±0.02s
scp       1.93±0.04s        2.72±1.00s      3.64±0.07s        3.41±0.02s
sy        2.57±0.15s        6.03±0.55s     55.85±1.54s        0.79±0.15s
pxs       6.45±0.05s        0.95±0.00s      2.88±0.01s        1.56±0.01s
fastest          cp2              pxs            rsync              cp2
```

The full suite — scripts, generated trees, and how to run it over a real
network — is documented in [`ARCHITECTURE.md`](ARCHITECTURE.md).

## How it works

A sync compares file manifests, plans create/update/delete, and sends only
the changed bytes — FastCDC content-defined chunks plus BLAKE3, so a 1-byte
edit in a 50 GB file transfers kilobytes. Files are applied in bounded
memory over a single pipelined stream, and the run's sessions multiplex
over one ssh connection. The wire protocol, deployment mechanics, and
security model are documented in [`ARCHITECTURE.md`](ARCHITECTURE.md).

## Contributing

Issues and pull requests are welcome — open an
[issue](https://github.com/elemeng/cp2/issues) or a
[pull request](https://github.com/elemeng/cp2/pulls).

- **Bug reports**: include the exact command, `cp2 --version` output, and
  the platforms involved (local and remote).
- **Before a PR**: `cargo test` must pass and
  `cargo clippy --all-targets -- -D warnings` must be clean.
- **Larger features** get discussed in an
  [issue](https://github.com/elemeng/cp2/issues) first.

## Acknowledgments

cp2's design, and parts of its code, are informed by the following projects.
None are linked at runtime — adaptations are credited per module in the
source headers, and their retained copyright lines live in
[`NOTICE`](NOTICE).

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
| msy | MIT | Sync-pipeline study |
| syncz | MIT | Sync-protocol study |

All adaptations are from MIT/BSD-licensed projects, compatible with cp2's
MIT license; rsync (GPL) contributed design and interface semantics only.

## License

MIT — see [LICENSE](LICENSE). Third-party adaptations and their retained
copyright lines are attributed in [NOTICE](NOTICE).