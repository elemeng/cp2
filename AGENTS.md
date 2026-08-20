# cp2 (MotorCycle branch)

Rsync-style file-sync tool over SSH with a rsync-style CLI. Single
`Cargo.toml`, one binary, no workspace overhead.

## Directory layout

```
cp2/
├── Cargo.toml              # Single package (no workspace)
├── src/
│   ├── lib.rs              # Library crate (engine API + CLI modules)
│   ├── main.rs             # Binary entry point
│   ├── cli.rs              # CLI argument definitions (clap)
│   ├── error.rs            # Error types
│   ├── commands/           # CLI commands (sync, server, watch)
│   ├── delta/              # FastCDC content-defined-chunk delta engine (pure)
│   ├── target/             # Sync targets (RemoteTarget/Location)
│   ├── platform/           # Portable fs primitives + staged-file sink + storage detection
│   ├── protocol/           # Wire format frames (version-only Hello, delta recipes)
│   ├── security/           # Path sanitization
│   ├── sync/               # scanner, linkpolicy, filter, planner, strategy, stats, sender, receiver, executor
│   └── transport/          # ssh spawner + bandwidth limiter
└── tests/                  # Integration tests (e2e push+pull over --server child, CLI smoke)
```

## Build

```bash
cargo build
cargo test
cargo clippy
```

## Design principles

- **Pure decision logic, thin async orchestration.** `delta/`, `sync/filter`,
  `sync/planner`, `sync/strategy` are pure (`#![forbid(unsafe_code)]`); only
  `sync/sender` and `sync/receiver` touch transport + delta. Clean boundaries:
  `transport` spawns the channel (no frame knowledge), `protocol` is the wire
  codec (no transport), `sync` decides what to send.
- **Unix rides system `ssh`; Windows rides russh.** The pure-Rust russh backend
  is the Windows default (OpenSSH `ControlMaster` breaks there), overridable
  with `CP2_TRANSPORT=ssh|russh`. `cp2 --server` runs on the sshd side; the
  client auto-deploys the matching binary to the remote (`--remote-path` /
  `--no-auto-install`). `--jump-host` = ProxyJump; `sudo`/password handled via
  `--remote-sudo` / `--sudo-password` / `--password`.
- **One sequential stream.** No QUIC/multi-stream/negotiation. The Hello is
  build-fingerprint-only (`BUILD_FINGERPRINT`, FNV-1a over sources), so any
  source change auto-redeploys and a mismatched peer fails the Hello.
- **FastCDC delta.** Content-defined chunking (`chunkrs`, 4/16/64 KiB) +
  BLAKE3 matching; `compute_delta` re-sends only changed chunks as literals,
  streaming with bounded memory (256 MiB literal cap, 1 MiB copy scratch).
  Large new files stream as `FileStart/Chunk/End` frames over a zero-copy wire
  layout. Basis signatures are requested on demand (groups of 32) and cached
  across runs (`sigcache`, keyed by size+mtime; `~/.cache/cp2/sig-cache`).
- **Cross-file delta.** Sibling files ("1.iso"/"1.1.iso", same dir, sizes
  within 2x) pair in the plan: the dependent ships as a delta against the
  reference copy — the scenario where CDC beats rsync.
- **Metadata & links.** `-a` is byte-identical: the `rlpt` core is always on,
  plus special files, owner/group (best-effort chown), SUID/SGID/Sticky, and
  literal symlinks. Link policy is decided at scan time (`--literal-*-links`,
  `--follow-links`, `--skip-links`); hard links are re-formed by inode.
- **Safety.** `--remove-source-files` / `--verify` hash on-the-fly (BLAKE3)
  with per-file fsync before the Ack; `--max-delete` aborts an oversized
  delete; `--existing`, `--ignore-times`, `--ignore-existing`, `--backup`,
  `--partial` follow rsync semantics.
- **Pipelined & storage-adaptive.** End-to-end pipelining keeps the wire fed;
  the apply window is 1 on HDD / 16 on SSD (`-j/--jobs` overrides).
- **Opt-in features.** `-z` = lz4, `-S` = sparse write, `-X` = xattrs,
  `-U` = atimes, `--bwlimit` = token bucket, `--fsync`, `-W/--watch` realtime
  (notify + debounce; pull is server-driven).
- Adapted from studied crates (copia, sparsync, sy, robosync, pxs), not linked.

## Public API (via the `cp2` library crate)

```rust
use cp2::{Executor, ExecutorOptions, Location};
use std::io::{AsyncRead, AsyncWrite};

// The executor runs the sync protocol over any byte stream.
let send: Box<dyn AsyncWrite + Unpin + Send> = /* ssh child stdin */;
let recv: Box<dyn AsyncRead + Unpin + Send> = /* ssh child stdout */;

let mut executor = Executor::new(send, recv);
let stats = executor.push("/path/to/dir", &ExecutorOptions::default()).await?;
```

## CLI usage

Rsync-style: `cp2 SRC DST`, direction inferred from which side is remote
(`user@host:path`; port set with `--port`, never in the target string).
A leading `/` after the colon is an absolute server path, otherwise relative
to the serve root (account home).

```bash
cp2 -p 2222 /path/to/dir "alice@127.0.0.1"          # push
cp2 -p 2222 "alice@127.0.0.1:backup" /path/to/restore  # pull
cp2 ./src ./dst                                     # local copy
cp2 /path/to/dir user@host --dry-run                # dry run
```

`cp2 --server` is the sshd-invoked server mode (not for direct use). The
client auto-deploys the matching binary to `~/.cargo/bin/cp2` by default.

Key flags: `-a/--archive` (byte-identical; the `rlpt` core is always on, `-a`
adds specials/chown/SUID-SGID-Sticky and implies `--literal-links`),
`-p/--port`, `--remote-path`, `--binaries-dir`, `--no-auto-install`,
`-n/--dry-run`, `-W/--watch`, `--delete`, `--max-delete`, `-u/--update`,
`-c/--checksum`, `--ignore-existing`, `--existing`, `--ignore-times`,
`--backup`, `--remove-source-files`, `--verify`,
`--exclude/--include`, `-j/--jobs`, `-z/--compress`, `--bwlimit`, `--fsync`,
`-v/--verbose`, `-q/--quiet`, `-S/--sparse`, `-X/--xattrs`, `-U/--atimes`,
the link switches (`--literal-links`/`--follow-links`/`--skip-links` +
granular `--literal-*-links`), and the `rlpt` opt-outs (`--no-recursive`/
`--skip-links`/`--no-perms`/`--no-times`). Defaults mirror `rsync -avP` (minus
`-z`): recursive, mode/mtime preservation, per-file listing, terminal progress,
keep-partials on abort. See `README.md` for the full table.

## Conventions

- `cargo test` runs lib + integration tests; keep the e2e push/pull tests
  passing (`tests/e2e_{transfer,links,meta,features}.rs` share helpers in
  `tests/common/mod.rs` and run against a spawned `cp2 --server` over pipes —
  no sshd needed in CI).
- Benchmarks live in `bench/` (sourced from `bench/lib.sh`, take
  `CP2_BIN`/`HOST`/`WORK`): `bench/mixed-tree.sh` (≈10 GiB / 100 K-file) and
  `bench/single-file.sh` (`MODE=large|small`).
- `cargo clippy` must be clean (`Cargo.toml` sets `clippy::all` deny,
  `pedantic` warn; CI runs `-D warnings`).
- Delta types are `serde`-serialized over the wire via `postcard`.
- The platform layer (`src/platform/`) is portable and dependency-free; auth
  and access control are sshd's, so nothing here is OS-specific (only the
  HDD/SSD probe differs by OS).
- `~/.cargo/registry` is always readable for reference source of the studied
  crates (copia, sparsync, sy, robosync, etc.).
