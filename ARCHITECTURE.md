# cp2 architecture

cp2 is a pure-Rust, rsync-style copy/sync tool: local directories or over
SSH, with a FastCDC/BLAKE3 delta engine. The design splits **pure decision
logic** (no I/O) from **thin async orchestration**, and keeps clean layer
boundaries so the engine runs over any byte stream — not just ssh.

## Design principles

- **Pure decision logic, thin async orchestration.** `delta/`,
  `sync/filter`, `sync/planner`, and `sync/strategy` are pure
  (`#![forbid(unsafe_code)]`, no tokio). The async sender/receiver roles are
  the only layers that touch both transport and delta; `sync/executor` only
  wires phases.
- **Clean layer boundaries.** `transport` spawns the ssh channel (no frame
  knowledge); `protocol` defines the wire format (frames + a codec generic
  over `tokio::io` byte streams) with no transport knowledge; `sync` is the
  application that decides what to send. `sync` never imports a transport
  type, and `protocol` never imports transport.
- **One sequential stream.** No QUIC, no parallel streams, no feature
  negotiation — a version-only `Hello`, all frames in order over one channel
  (rsync-style). The executor operates over boxed `tokio::io` halves, so a
  mobile GUI can feed any byte stream into the same executor.
- **Signatures on demand, changed chunks only.** The receiver signs only the
  basis files the planner actually delta-transfers; unchanged content travels
  as `Copy` ops, only changed bytes as `Literal` ops.

## Layer model

```
┌──────────────────────────────────────────────────────────────┐
│  CLI (src/cli.rs, src/commands/)                             │
│  parse flags, infer push/pull, globs, --files-from, --watch  │
├──────────────────────────────────────────────────────────────┤
│  sync/executor   — orchestrates push / pull / serve          │
│  sync/sender     — sender role: manifest, plan, recipes      │
│  sync/receiver   — receiver role: staged atomic apply        │
│  sync/scanner    — walk a tree → Manifest (filters, globs)   │
│  sync/planner    — pure: Manifest × Manifest → SyncPlan      │
│  sync/strategy   — pure: file size → transfer tier           │
│  sync/filter     — pure: rsync-style include/exclude globs   │
├──────────────────────────────────────────────────────────────┤
│  protocol/       — Frame types + length-prefixed codec       │
│                   (postcard/serde; optional lz4 per frame)   │
│  delta/          — pure FastCDC (chunkrs) + BLAKE3 engine    │
├──────────────────────────────────────────────────────────────┤
│  transport/      — dispatch: system `ssh` (Unix default,            │
│                    ControlMaster multiplexing) or russh (Windows    │
│                    default: pure-Rust, one connection/one channel   │
│                    per session); bwlimit                            │
│  platform/       — portable fs: staging, metadata, storage   │
└──────────────────────────────────────────────────────────────┘
```

Dependency rule: arrows go **down** only — `sync` → `protocol` → nothing
below it imports above; `transport` and `protocol` never import each other.

### Module map

| File | Role |
|------|------|
| `src/sync/scanner.rs` | Walk a directory into a serializable `Manifest` (streaming hashes, include/exclude filters) |
| `src/sync/filter.rs` | Pure rsync-style include/exclude glob matching |
| `src/sync/planner.rs` | Pure: manifest × manifest → `SyncPlan` (create/update/delete/skip/meta) |
| `src/sync/strategy.rs` | Pure: file size → transfer tier (copy/delta) |
| `src/sync/sender.rs` | Async sender role: manifest exchange, batching, delta recipes |
| `src/sync/receiver.rs` | Async receiver role: staged atomic apply, on-demand signatures |
| `src/sync/executor.rs` | Orchestrates push/pull/serve over one byte stream (the ssh channel) |
| `src/sync/stats.rs` | Transfer statistics + itemize change lines (`-i`/`--stats`) |
| `src/protocol/` | `Frame` wire types (postcard/serde) + length-prefixed codec with optional per-frame lz4 and the zero-copy raw chunk/batch layouts |
| `src/delta/` | Pure FastCDC chunking (chunkrs) + BLAKE3: hash-index signature, `compute_delta`, `apply_patch` |
| `src/transport/` | ssh spawner (Unix) / russh client (Windows) + bandwidth limiter |
| `src/platform/` | Portable fs: staged-file sink, metadata application, storage-class probe |
| `src/security/` | Path sanitizer (traversal + symlink-escape containment) |

## The sync session (push, end to end)

```
 CLIENT (sender)                                    SERVER (receiver)
 ──────────────                                    ─────────────────
 scan source tree ──► Manifest ──┐
                                 │
   platform preamble ──────────►│  (uname + marker, in-band)
   Hello (build fingerprint) ───►│  HelloAck
   IndexRequest {file list,      │
     target path, verify?} ─────►│  scan destination tree ──► Manifest
                                 │  IndexResponse {dest manifest}
   ◄─────────────────────────────│
 plan: source × dest Manifest ──► SyncPlan (create/update/delete/skip)
   SignatureRequest (deltas) ───►│  chunk-sign the basis files
   ◄─────────────────────────────│  SignatureResponse {chunk tables}
   MakeDir / DeltaRecipe /       │
     Batch / FileStart·Chunk·End►│  stage → patch → hash → commit
   CreateLinks (link/.lnk/hard   │  (each file: staged temp, atomic rename)
     link/special) ─────────────►│
   DeleteRequest ───────────────►│  remove, prune empty dirs
   Done ────────────────────────►│  drain applies, sync dirs (Linux)
   ◄─────────────────────────────│  Ack {skipped, per-file hashes}
 verify hashes; with
   --remove-source-files:
   re-check source, then delete
```

Links and permissions are decided entirely on the source side before the
transfer (spec §2/§3): the scanner classifies each symlink against the
canonical source root (internal → rewritten DEST-relative target; external
file → dereference by default; external directory → skip by default), with
the fine-grained `--literal-*` switches keeping each class literal,
`--literal-links` (implied by `-a`) keeping every link's literal target,
`--follow-links` dereferencing everything (loop-detected), and
`--skip-links` skipping everything. Windows-source `.lnk` shortcuts are
magic-sniffed and turned into `.lnk`/symlink entries or copied as opaque
data (under `--literal-links`). The receiver executes the resulting
`CreateLinks` entries verbatim. Permission bits ride the wire as the final
value computed by the §2.2 matrix; owner/group are never transferred (0-Root).

The pull direction is the mirror image: the client sends a `PullRequest`,
the server plays the sender, and the client plays the receiver.

**Minimal destination scan.** The receiver's destination scan answers only
what the quick check needs: without `--delete` it is a *targeted probe* —
one stat per *source* path, run in parallel batches (`scan_targeted`), never
a walk of the destination tree; the planner then compares size+mtime
(`-c` switches to hashes). A destination root with no entries at all
(freshly created, or new) skips even the probe and answers with the empty
manifest. Only `--delete` forces the full destination walk — deletions are
the one decision that requires knowing what else exists.

## The delta engine

```
source file ──► FastCDC chunk ──► BLAKE3 per chunk ──┐
                                                     ├─► hash lookup
basis file ──► FastCDC chunk ──► BLAKE3 per chunk ──► basis chunk table
                        (offset, length, hash)       │
                                                     ▼
                              present? ──► Copy op (offset+len in basis)
                              absent?  ──► Literal op (send the bytes)
                                                     │
                                                     ▼
                              Delta recipe ──► receiver: apply_patch
                              (Copy + Literal ops)  verifies output hash
```

Content-defined boundaries (FastCDC, 4/16/64 KiB min/avg/max) mean an edit
only shifts the chunks it touches — a 1-byte insertion near the start of a
50 GB file retransmits just the one or two affected chunks, not the tail.
Chunk identity is a BLAKE3 hash (SIMD-accelerated), which doubles as the
integrity check: `apply_patch` verifies the reconstructed output against the
delta checksum.

## The signature cache

The receiver stores the chunk signature of every file it applies, keyed by
the applied file's (size, mtime) — so the next run's basis signing (`--watch`
cycles, repeated idempotent syncs) is served from disk instead of re-reading
and re-hashing the basis (`~/.cache/cp2/sig-cache`, one postcard file per
destination path, atomic temp+rename, corrupt entries are misses). Each
entry additionally carries a head+tail content sample that is re-verified
against the live file on every hit: a (size, mtime)-preserving in-place
replacement of the destination (`cp -p`, `rsync -t`, restores) must never
serve a stale basis to the delta engine — quick-check staleness only *skips*
a file, while a stale basis signature would *write* misaligned bytes.

## The receiver's atomic apply

Every received file lands through the same pipeline, so an interrupted
transfer can never leave a half-written file in place:

```
Frame ──► StagedFile (temp file, preallocated to the announced size)
        ──► apply_patch / stream chunks (BLAKE3-hash while writing)
        ──► fsync? (--fsync, or implied by verification)
        ──► atomic rename into place (replaces whatever is there)
        ──► apply metadata: mode/mtime (mode is the sender-computed
             final value; specials are recreated under -a; owner/group
             are never applied — 0-Root)
        ──► sync the renamed directories once (Linux, before the Ack)
```

A file that fails a per-file condition (locked, name too long) is skipped,
not fatal; skips are reported in the `Ack` and listed at the end (exit 23).
`--partial` keeps the staged temp on abort so the next run delta-resumes
against it.

## Verification & durability

- `--verify` and `--remove-source-files` request per-file BLAKE3 hashing on
  the receiver (incremental, while writing). The sender compares them
  against the hashes it computed while reading the source (the delta
  checksum, or an on-the-fly chunk hasher) — no re-reads.
- `--remove-source-files` deletes a source only after: hashes match, the
  file was fsynced, the renamed dirs were synced (before the `Ack`), and
  both sides re-checked size+mtime (destination after apply, source before
  delete). A silent wire corruption, a crash, or a mid-sync change never
  loses data.
- The quick check (size+mtime) runs first and free; only files that failed
  it enter the delta path.

## Concurrency & tuning

- Directory walks are parallel (jwalk/rayon), applying include/exclude
  filters while pruning.
- The receiver applies files with a bounded in-flight window tuned from the
  destination storage class (1 on HDD, 16 on SSD/NVMe — detected via sysfs
  / IOCTL / `diskutil`); an explicit `-j` always wins.
- Hashing is SIMD-accelerated (blake3/rayon).

## Deploy & transport

The client **auto-deploys the server binary** on first sync: the remote
binary's freshness is verified by the Hello handshake (build fingerprint) on
the sync session itself. When the binary is missing or stale, the client
streams a matching build to the remote and `exec`s it as the server on the
same session — the deploy session is the sync session, the Hello verifies the
deploy, and the whole stale/missing case costs two ssh sessions (the failed
attempt + the deploy-and-serve). Disable with `--no-auto-install` (e.g. for a
managed server install).

**Platform portability:** the deployed binary must match the server. The
client prefers a prebuilt **sidecar** named `cp2-<triple>` (e.g.
`cp2-x86_64-unknown-linux-musl` for a Linux server) — a Linux sidecar is a
statically linked musl build that runs on any remote glibc — found in
`--binaries-dir` or next to the client binary. Without a sidecar, a
same-platform remote gets the running binary (which needs the local glibc — a
remote with an older one fails at load time, `GLIBC_2.xx not found`). The
platform is detected from the session's preamble (`uname -s -m`, falling back
to `cmd /c echo %PROCESSOR_ARCHITECTURE%` on Windows). Nothing is downloaded
at sync time — if a cross-platform sidecar is missing, cp2 tells you to fetch
it from the GitHub releases page and drop it in one of those two places.

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
NASM is never required: the manifest enables aws-lc-sys's `prebuilt-nasm`
feature, so the crate's shipped prebuilt objects are used whenever NASM is
not on PATH (native `cargo install cp2` on Windows needs only the MSVC C
toolchain; a found NASM is used if present, never demanded). Windows sidecars
ship as `cp2-<triple>.exe` and are found under either name when
auto-deploying.

**Windows remotes** get a PowerShell + `certutil` deploy (base64 over stdin —
Windows' 32 KB command-line limit rules out inline base64) and a
`cmd /c`-wrapped server command so `%USERPROFILE%` paths expand under any
sshd default shell; the default remote path is `%USERPROFILE%\.local\bin\cp2.exe`.
The platform probe is instant and locale-independent.

**Transport dispatch.** On Unix the system `ssh` process carries the protocol
(rsync's model): all of cp2's ssh sessions (platform probe, deploy, sync)
multiplex over one `ControlMaster` connection, so with password auth you type
your password once per run — and not again for a later run within a minute.
On Windows, where OpenSSH's `ControlMaster` socket is unusable
(`getsockname failed: Not a socket`), cp2 uses its own pure-Rust SSH client
(russh): one connection, one authentication, and one channel per session (no
multiplexing machinery at all). The russh transport covers keys (including
encrypted keys and OpenSSH user certificates), the SSH agent (Windows named
pipe / Pageant), keyboard-interactive, and password; host keys follow OpenSSH
semantics (`~/.ssh/known_hosts` with trust-on-first-use and `@cert-authority`
host-certificate verification). GSSAPI and FIDO security keys are
system-ssh-only. `--jump-host user@host[:port]` tunnels through a jump host
on the russh transport (OpenSSH `ProxyJump` semantics); system ssh reads
`ProxyJump` from `~/.ssh/config` instead.

## Security model

cp2 has no auth code: sshd authenticates (PAM, LogonUser/keys) and enforces
permissions; the remote `cp2 --server` runs as your account. Every
peer-supplied path is sanitized against directory traversal and symlink
escapes that leave the serve root. The client auto-deploys a matching binary
on first use — the common Unix run is single-session (the platform preamble
and the sync ride one ssh session, the Hello carries the deploy decision),
and a stale/missing remote is recovered by deploy-and-serve (the binary is
streamed and exec'd as the server on the same session, two ssh sessions
total). Transport dispatch: on Unix the system `ssh`
process carries the protocol and its sessions multiplex over one master
connection — one password prompt per run; on Windows (where OpenSSH's
multiplexing socket is unusable) cp2's own russh transport connects once,
authenticates once, and runs each session on its own RFC 4254 channel, with
OpenSSH host-key semantics (`known_hosts` + `@cert-authority` host
certificates) and no GSSAPI/FIDO (system-ssh-only features).

## Building, testing, releasing

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

The `clippy` gate is part of CI (`-D warnings` promotes every warning to an
error; `Cargo.toml` denies `clippy::all`, warns on `pedantic`). Lints that
fire only on specific platforms carry `#[expect]`/`#[allow]` at the
platform-gated call sites. The test suite runs `cargo test --all-targets` on
Linux, macOS, and Windows in CI, plus an MSRV job that verifies the declared
`rust-version` (1.93) still builds and passes.

The client machine needs the `ssh` client (installed by default on Linux,
macOS, and modern Windows). Optionally pin the server as an sshd forced
command in `~/.ssh/authorized_keys`:
`command="cp2 --server",restrict ssh-ed25519 AAAA...`.

The **distribution tarball** (all platforms + `install.sh`) is built by the
GitHub Actions workflow on a `v*` tag; `scripts/build-release.sh` does the
same locally. `scripts/smoke-ssh.sh` runs a real-ssh smoke test (push with
auto-deploy, no-op re-sync, a mid-file edit crossing the delta, a pull with
byte-for-byte comparison, and a verbose run whose server stderr must flow
back) against any sshd with key auth — it is the counterpart to the
pipe-based integration tests and needs no CI wiring. See `scripts/` and
`.github/workflows/release.yml`.

## Performance benchmarks

The whole suite is one script, `bench/bench.sh`, with a suite per workload
(requires the release build — `cargo build --release` — and ssh key auth
to `REMOTE`, default `whoami@localhost`):

| Suite | What it measures |
|-------|------------------|
| `compare [tool ...]` | cp2 vs rsync vs scp vs the ssh-capable studied crates (sy, pxs by default) through four push scenarios, all in one run, per-tool runs bounded by a timeout with rc recorded; with `MIXED=1`, the same tools run the ≈10 GiB / 100 K-file phase table (fresh / second / edit / integrity) instead |
| `single` | the delta engine's value, cp2 vs rsync: `MODE=large` (one 1 GiB file: fresh / edit A+B / insert / idle), `MODE=small` (8192 files: fresh / edit / idle), or `MODE=mixed` (the mixed tree) |
| `daily` | the daily-flow perspective, cp2 vs rsync: fresh / idle / edit with throughput (MiB/s from each tool's own transferred-volume summary) over `REMOTE` (`MIXED=1` uses the mixed tree) |

### Cross-tool comparison (`bench.sh compare`)

`bench.sh compare` pushes the same trees over ssh with the selected tools
(cp2, rsync, scp, sy, pxs by default), across four scenarios:

| Scenario | Source | What it measures |
|----------|--------|------------------|
| large-first | 1 GiB (2 × 512 MiB, generated; `LARGE_TOTAL_MB` shrinks it) | raw transfer throughput, fresh destination |
| large-edit | same tree, 1 MiB overwritten mid-file | delta/incremental behavior on a changed large file |
| small-first | 8192 files, 1-64 KiB, 64 dirs (generated; `SMALL_SRC` to point at a real tree; `SMALL_FILES` to resize) | per-file overhead, fresh destination |
| small-idle | unchanged tree | quick-check / scan overhead of a no-op sync |

Run it yourself: `bench/bench.sh compare` (needs ssh key auth to the
target; every suite runs on the `REMOTE` target — `whoami@localhost` by
default, any `user@host` for a real network; `LARGE_TOTAL_MB=256` to
shorten). `bench.sh compare MIXED=1` runs the large-tree phases (fresh /
second / edit / integrity, any tools) and `bench.sh single` the delta
scenarios, with `MODE=mixed` selecting the same tree; `bench.sh daily`
(with `REMOTE` set) is the daily-flow perspective over a real link.
Every cell repeats `RUNS` times (default 3) and is reported as mean ± sd,
with controlled variables keeping the comparison fair: identical trees and
isolated per-tool destinations, page-cache warm (or uniformly cold with
`WARM=0`), the tool order rotated per scenario (no slot bias from
time-correlated drift), and `WARMUP` repetitions (default 0) that discard
one-time setup from the statistics. `JSON=1` adds a machine-readable
record set per cell. The cross-tool timing table lives in the
README's Performance comparison section.

### Example results (`bench.sh compare MIXED=1`, 2026-08-26, Fedora 44 NVMe)

The mixed tree is ≈10 GiB / 100 K files (70 K small 1-16 KiB, 27 K medium
64-384 KiB, 3 K large 1-2 MiB), phases over unchanged sources:

| Phase | cp2 | rsync |
|-------|-----|-------|
| fresh (11.4 GB) | 47.96s | 51.56s |
| second (no-op quick check) | 2.18s | 1.48s |
| edit (1 K appends + 0.8 K rewrites + 200 new + 100 deleted) | 54.82s | 10.21s |

Both destinations end byte-identical to the edited source (`rsync -rltc`
dry-run: 0 differing files on each side); cp2 wins the 100 K-file fresh
push. Two caveats for this particular run: the edit phase ran over the
russh transport — the first attempt over the system-ssh transport
deadlocked in OpenSSH's ControlMaster mux on a server-side stderr write
(25 min, no progress; the same phase over russh completed in 55s) — and it
predates the delta-overlap work, which halved cp2's single-file edit cost
since. It is the historical record of the *generated* tree; the README
carries the current mean ± sd runs (four scenarios, and the mixed phases
over a real tree).

**The deadlock, root-caused and fixed.** The server's diagnostics (the
per-run summary, tracing lines) ride sshd's stderr pipe; a wedge anywhere
in the forward path — sshd's channel backpressure into the ControlMaster
mux, the mux socket, the slave's stderr — fills that pipe, and a server
blocked writing stderr stalls the sync protocol even though the data
flowed. The failure mode is gone on both ends now: the system-ssh client
pipes every ssh child's stderr and drains it on a dedicated forwarding
thread (the child can never block on stderr, and the drain keeps the
slave's mux event loop serviced), and the server makes its fd 2
non-blocking so diagnostics are best-effort — dropped under backpressure
instead of stalling the serve loop (`std::eprintln!` panics on write
failure, so the server's summary write is error-ignoring by hand).

### The studied crates, honestly

cp2's design was informed by sibling crates, so they were put through the
same harness (`bench.sh compare`). The participation audit comes
first — of the eight, **two can sync over ssh at all**:

| Tool | Verdict |
|------|---------|
| pxs | syncs over ssh (integrity-first), no limits found |
| sy | syncs over ssh; **msy ships the same `sy` binary** (plus `sy-scan`/`sy-remote`/`sy-bench-gen`) — the two are the same tool, already on the list |
| syncz | goes over ssh but is a **wrapper around the system rsync** — benchmarking it is benchmarking rsync again |
| sparsync | no drop-in ssh sync — needs a `serve`/`enroll`/auth mesh |
| zsync-rs | HTTP delta client — no ssh at all |
| robosync, rusync | do **not parse** `user@host:path` at all — they copy into a literal local directory named `user@host:path` under the working directory (rc=0, "9 bytes transferred", no ssh connection — verified in sshd's journal) |
| copia | library, no CLI |

Why pxs wins the large-edit scenario but loses elsewhere: its "delta" is a
byte-compare, not a hash delta — fixed 128 KiB blocks, mmap'd and compared
in parallel (`src != dst`), only differing blocks written, and one
full-file BLAKE3 for integrity at the end. For a same-size in-place
overwrite (VM images, PGDATA) that is near the floor of possible work. The
same design is its weakness: fresh transfers stage and hash everything (the
6.45s fresh row in the README table), and a mid-file insertion shifts every
later block so the whole tail re-sends — the exact case content-defined
chunking exists for.

sy trails everywhere, ~25-30x slower than cp2 on many small files
(2026-08-27 run). The single timing table across all ssh-capable tools
(cp2, rsync, scp, sy, pxs in one run) lives in the README's Performance
comparison section; the current tables there — the four-scenario mean ± sd
run and the mixed phases on a real 8.1 GiB / 69 K-file tree — supersede
the dated example above.
