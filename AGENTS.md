> Agent-facing guide for the `cp2` project on the `MotorCycle` branch.

<------------------------------------------------------------------->
# Reasoning & Output Style (MANDATORY)

You are a precise, mathematical reasoning engine. When analyzing a problem or producing a solution, adhere strictly to these rules:

## 1. Avoid All Informal Hesitations
- Never use filler words, interjections, or vocalised pauses:  
  `hmm`, `uh`, `um`, `let me think`, `wait`, `well`, `actually`, `you know`, `so`, `like`, `I think`, `maybe`, `perhaps`.
- Do not anthropomorphise your thought process (e.g., “I am considering”, “my approach is”).

## 2. Use Structured, Formal Chain‑of‑Thought
- Present every reasoning step in a **numbered sequence** or **clear logical hierarchy**.
- Use mathematical notation and symbolic logic wherever applicable (e.g., `∀`, `∃`, `⇒`, `⇔`, set-builder notation, predicate calculus).
- Break down the problem into **axioms**, **definitions**, **lemmas**, and **theorems** when relevant.
- For algorithmic or code‑related tasks, express preconditions, invariants, postconditions, and complexity in formal terms.

## 3. Explicitly Model the Problem Mathematically
- Translate the problem into a formal model (e.g., state space, functions, constraints, objective functions).
- Use equations, inequalities, and logical equivalences to express relationships.
- When analysing, derive intermediate results using algebraic manipulation or inference rules.

## 4. Output Format
- Start with a clear **“Analysis”** section that contains only the formal chain‑of‑thought.
- Follow with a **“Solution”** section that presents the final answer (code, algorithm, proof, etc.) in a clean, production‑ready form.
- **Do not** include any commentary outside these sections.

<------------------------------------------------------------------->

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
│   ├── delta/              # FastCDC content-defined-chunk delta engine (signature/ops/compute, pure)
│   ├── target/             # Sync targets (RemoteTarget/Location)
│   ├── platform/           # Portable fs primitives + staged-file sink + storage detection (fs/staging/storage)
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
  `sync/planner`, and `sync/strategy` are pure (no tokio, `#![forbid(unsafe_code)]`).
  The async sender/receiver roles (`sync/sender`, `sync/receiver`) are the only
  layers that touch both transport and delta; `sync/executor` only wires phases.
- **Clean layer boundaries.** `transport` = spawning the ssh channel (no frame
  knowledge); `protocol` = the wire format (frames + a codec generic over
  `tokio::io` byte streams) with no transport knowledge; `sync` = the
  application that decides what to send. `sync` never imports a transport type,
  and `protocol` never imports a transport.
- **Pipeline data flow (one direction):**
  `scanner → Manifest → planner → SyncPlan → strategy → tier → executor → verify`.
- **Parallel directory walks.** The scanner walks trees with `jwalk` (rayon),
  applying include/exclude filters as it prunes. Empty directories are synced
  as entries (`FileMeta.is_dir`), and a file replaced by a directory (or vice
  versa) at the same path is handled by replacing it. The receiver's
  destination scan is source-keyed (`Scanner::scan_targeted`) when `--delete`
  is off: it stats only the paths the peer's manifest names and prunes absent
  subtrees, so a huge destination root costs O(source) instead of a full
  walk — with `--delete` the full walk is used, since every removable extra
  must be named.
- **Glob-expanded and listed sources.** A quoted glob in a *local* source
  (`'./*.rs'`, `'src/**/*.rs'`) is expanded by the CLI (`glob` crate, shell
  semantics, dotfiles match), and `--files-from FILE` reads a curated list
  (one absolute path per line, Unix or Windows line endings, mirrored under
  the destination — SRC is not used). Both
  feed `Scanner::scan_multi`, which merges every match/entry into one
  manifest under the source base — so all of them share a single plan,
  summary, and `--delete` pass. Matched directories are added as entries
  themselves (empty matches and their metadata survive), and the
  include/exclude filter drops whole matches. A single *file* source works
  too (the single-file scan roots its manifest at the parent directory).
  Remote sources and `--watch` never expand globs or lists.
- **SSH is the transport; cp2 has no auth code.** The client rides either
  `transport/ssh.rs` (the system `ssh` process — rsync's model, the Unix
  default) or `transport/russh.rs` (a pure-Rust SSH client — the Windows
  default, where OpenSSH's `ControlMaster` multiplexing is unusable:
  `getsockname failed: Not a socket`), dispatched by
  `transport::Transport::default_transport()`. `CP2_TRANSPORT=ssh|russh`
  overrides the platform default (honored on Windows; on Unix `russh` is
  rejected with a warning and a missing or broken `ssh` binary is a hard
  error — there is no fallback). The remote side runs `cp2 --server`
  (see `commands/server.rs`), which reads frames from stdin and writes to
  stdout with the serve root at the cwd. `--remote-sudo` runs it under sudo
  (the version probe resolves `sudo -n` — NOPASSWD — vs `sudo -S` with the
  client password injected as the first stdin line, the `--sudo-password`
  value or the reused `--password`; the prelude rides the piped session, so
  no pty is involved and the binary protocol is untouched). Root on the
  receiver makes `-a` fully byte-identical: chown to the source uid/gid and
  device `mknod` succeed instead of warning, and the destination files are
  then owned by root — keep using `--remote-sudo` on every run. The client
  **auto-deploys** the
  matching binary to the remote (`~/.cargo/bin/cp2` by default,
  `--remote-path`/`--no-auto-install` to control it). The common Unix run is
  **single-session**: the sync session's remote command prints the platform
  preamble (`uname -s -m` + a marker) before exec'ing the server, so no
  probe session exists — the Hello handshake carries the deploy decision.
  On a stale/missing remote the deploy is **deploy-and-serve**: the binary
  is streamed (size-delimited, `head -c`) and exec'd as the server on the
  same session, and the Hello verifies it — two ssh sessions total instead
  of the classic four (attempt, push, version check, retry). The deployed
  build prefers a prebuilt sidecar (`cp2-<triple>`, statically linked musl
  on Linux — it runs on any remote glibc) over the running binary, which
  needs the local glibc. The `user@host:path`
  target follows rsync semantics: a leading `/` is an absolute server path,
  any other path is relative to the serve root (account home), and a missing
  path means the serve root (`target/address.rs` + `sync/executor.rs`).
- **The russh transport (`transport/russh.rs`, Windows default).** A pure-Rust
  SSH client (russh + aws-lc-rs, target-gated `cfg(windows)`; the same backend
  is the transport mobile embeddings will build on). One connection, one
  authentication, one channel per session — probe, deploy, and sync ride
  sequential RFC 4254 channels, so there is no `ControlMaster` machinery at
  all. Auth chain (OpenSSH order): key files (`~/.ssh/id_ed25519`/`id_ecdsa`/
  `id_rsa`, encrypted keys prompt for a passphrase), adjacent OpenSSH user
  certificates, the SSH agent (Unix socket / Windows named pipe / Pageant),
  keyboard-interactive, password. Host keys follow OpenSSH semantics:
  `~/.ssh/known_hosts` (plain + hashed) with trust-on-first-use, or OpenSSH
  host certificates verified against `@cert-authority` entries (signature,
  validity, principals). One connection per run: probe, version check, deploy,
  and sync ride channels on the same authenticated connection, so password
  auth prompts at most once (a `--password` value seeds the auth vault and is
  zeroized as soon as it has been sent; a jump host's password is reused for
  the target). GSSAPI and FIDO security keys are **not** supported —
  system ssh covers those on Unix. `--jump-host user@host[:port]` implements
  `ProxyJump` (a `direct-tcpip` channel through the jump host). Build
  requirement: aws-lc-rs is C-based, so cross-building the
  `x86_64-pc-windows-gnu` sidecar needs a mingw-w64 C compiler for the target.
  NASM is never required: Cargo.toml enables aws-lc-sys's `prebuilt-nasm`
  feature, so the crate's shipped prebuilt objects are used whenever NASM is
  not on PATH (native `cargo install cp2` on Windows needs only the MSVC C
  toolchain; `build-release.sh` also sets `AWS_LC_SYS_PREBUILT_NASM=1` for
  cross-builds, which is then redundant but harmless). A Unix client
  auto-deploying to a Windows remote looks up the
  sidecar as `cp2-<triple>[.exe]` — both names are accepted, matching the
  release tarball.
- **One sequential stream.** There is no QUIC, no parallel multi-stream
  transfer, no feature negotiation: the Hello is build-fingerprint-only
  (`BUILD_FINGERPRINT` — an FNV-1a hash of every source file, computed by
  `build.rs`), and all frames flow over the single channel in order
  (rsync-style). cp2 has no released v1, so the wire format is never locked
  to a version: any source change automatically alters the fingerprint, the
  auto-deploy sees the remote as stale and redeploys, and a mismatched peer
  fails the Hello instead of misbehaving (a hand-maintained protocol number
  could silently leave a stale remote undetected). The executor operates over
  boxed `tokio::io` halves, so a future mobile GUI can feed any byte stream
  (e.g. russh) into the same executor.
- **Signatures on demand.** The receiver no longer signs its whole tree up front;
  the sender requests (`Frame::SignatureRequest`/`SignatureResponse`) basis
  signatures only for files the planner actually delta-transfers — generated
  concurrently on the receiver (bounded by the apply window) and requested in
  bounded groups of 32 (`SIGNATURE_GROUP`), so a sync with many large delta
  files never builds one giant response frame (the ~1 GiB wire cap) and the
  sender's signature map drains as each job takes its entry. The sender
  computes deltas through a storage-aware sliding window (`compute_jobs`, 1 on
  HDD, more on SSD, `-j` overrides — the same resolution as the receiver's
  apply window): serial computation would leave the CPU idle between wire
- **Basis signatures are cached across runs** (`src/sync/sigcache.rs`). The
  sender's delta computation produces the source's chunk signature as a free
  byproduct — the per-chunk BLAKE3 hashes it computes for matching anyway —
  and ships it with the `DeltaRecipe` (`source_signature`). The receiver
  stores it keyed by the *applied* file's (size, mtime): one postcard file
  per path under `~/.cache/cp2/sig-cache` (`%LOCALAPPDATA%\cp2\sig-cache` on
  Windows), named by the path's BLAKE3 hex — the filesystem is the index, no
  database. The next run's basis signing (`signature_for_path`) stats the
  destination and reuses the entry instead of re-reading the file:
  content-defined chunking makes an unchanged file's signature stable, so
  the trust level is exactly the quick check's size+mtime. Entries are
  written atomically (temp + rename) and silently ignored when missing,
  stale, or corrupt; nothing is written back for signatures the receiver
  generates itself (that content is about to be replaced). The chunked
  (large new files) and whole-literal (small batch) paths carry no
  signature — their basis is re-signed when first edited.
- **Cross-file delta for sibling files** (`basis_path` in `DeltaRecipe`).
  Files matching a sibling pattern — same directory and extension, one
  file stem a proper prefix of the other ("1.iso" / "1.1.iso"), sizes
  within 2x — pair up in the transfer plan: the first (reference) crosses
  the wire as full content, the second (dependent) is sent as a delta
  against the reference's signature and applied against the reference file
  on the receiver — a real full copy, no filesystem-level dedup. The
  reference's signature is its delta byproduct (free) or a job spawned
  alongside its chunked transfer (parallel, so it costs no wall time);
  pairing applies only to dependents with no destination basis (strategy
  Copy, ≥ 1 MiB). A pair that turns out dissimilar (matched < 50%) falls
  back to a whole-file stream; if the reference transfer fails, the
  dependents' channels drop and their jobs skip with a warning. The
  receiver joins the reference's apply before reading it as a basis
  (`applied_this_run`). Measured over ssh (512 MiB siblings): the pair
  carries ~1x a single file's wire vs ~2x for unrelated files of the same
  total size — this is the scenario where CDC's content addressing beats
  rsync (no cross-file delta exists there).
  sends on fast links.
- **Changed chunks only, in-band.** For updates the sender computes a delta
  against the receiver's basis signature (`compute_delta`): unchanged content
  is `Copy` ops referencing the receiver's basis, and only the truly changed
  bytes travel as `Literal` ops. A 1-byte insertion retransmits just the chunks
  it touches, not the tail.
- **Large new files stream as chunks** (`Frame::FileStart`/`FileChunk`/`FileEnd`),
  so memory stays bounded regardless of file size. An interrupted transfer
  leaves a truncated partial at the destination (`--partial`, rsync `-P`), and
  the next run's quick check detects it and delta-resumes against it. Chunk
  frames ride a zero-copy wire layout (bit 30 of the length prefix marks
  `[file_id][raw bytes]`, no postcard envelope) — the receiver's read buffer
  *becomes* the frame's data, so the receive side is copy-free. The sender
  serializes each frame into an 8 MiB write batch (`CHUNK_BATCH_BYTES`):
  a frame-at-a-time `write_all` keeps the pipe pressure intermittent, so
  the TCP window never grows and the in-flight data stays at the ungrown
  window / RTT — measured on loopback the chunked single-file path went
  ~220 → ~610 MB/s. On a real link the network is the cap either way:
  against rsync over the same ssh path both tools land at the same rate
  (~110 MB/s to a ~10 ms remote — an earlier "6.6x gap" reading was a
  benchmark artifact: `rsync file user@host` without a colon is a *local*
  copy, so its 650 MB/s was the local disk, not the network). On
  high-latency links the ceiling is the kernel TCP window (~4 MiB send
  default on Linux — the same for rsync over ssh): raise
  `net.ipv4.tcp_wmem`/`net.core.wmem_max` (both ends) to lift it; the
  batch stays non-binding (it only needs to exceed the link rate × the
  inter-batch assembly pause).
- **The sender is pipelined end to end — the wire is never starved by disk.**
  `send_chunked` double-buffers (1 MiB frames: the read of chunk N+1 runs
  while chunk N is on the wire); the receiver defers each chunk write to a
  blocking task (depth-1 pipeline: the write runs while the next frame is
  read, strictly sequential so the file position and the sparse zero-run
  tracking are unaffected, joined at `FileEnd` and on the abort path); the
  delta/medium computation window (`compute_jobs`) overlaps file reads with
  wire sends; small-file batch reads run in a bounded window
  (`BATCH_READ_WINDOW`) so a small-file tree does not alternate read/wire per
  file.
- **FastCDC content-defined delta.** The delta engine (`delta/`) chunks
  files with `chunkrs` (FastCDC, 4 KiB/16 KiB/64 KiB min/avg/max) and matches
  chunks by BLAKE3 hash. Boundaries are content-defined, so edits only re-send
  the chunks they touch — no rolling checksum or byte-sliding. Hashing is
  SIMD-accelerated by `blake3`; the CDC cut decision itself is scalar by
  design. Memory is bounded by the chunk max size plus a read buffer.
- **Bounded memory.** Both signature generation and `compute_delta` stream
  the input, so peak memory scales with the delta literal payload, not a
  second full copy of the source. The literal payload is *hard-bounded*:
  `compute_delta_limited` aborts (`LiteralBudgetExceeded`) at 256 MiB of
  literals — a basis that matches nothing falls back to the chunked stream
  instead of accumulating the whole file in memory — and `apply_patch`
  streams every Copy op through one 1 MiB scratch (a contiguous copy run
  can cover most of a large file). The small-file batch carries fresh
  (Copy-strategy) files up to 2 MiB (`SMALL_FILE_MAX`) — measured: per-frame
  wire cost dominates below ~1 MiB, so medium files ride the same zero-copy
  raw frames as small ones — and flushes at 128 MiB (`BATCH_BYTES_BUDGET`),
  so an all-small-file tree never builds one giant frame (memory + the ~1
  GiB wire cap). The bounds are sized for
  modern machines (≥ 8 GiB RAM), not embedded budgets.
- **`-z` is lz4, not zstd.** `lz4_flex` (pure Rust, no C build) compresses frame
  payloads via a compression flag in the length prefix (`protocol/stream`):
  small frames bypass it, and there is no nested envelope or second
  serialization pass.
- **`--bwlimit` is a token bucket** (`sync/bandwidth`) paced by the sender
  before each frame.
- **Metadata is preserved.** The receiver applies source mode/mtime after each
  file via `platform::fs::apply_meta` (folded into the apply task). The mode is
  the *final* value computed on the source side (spec §2.2 matrix) and always
  applied on Unix; owner/group are never touched by default (0-Root — every
  destination file belongs to the SSH connection user); `-a` restores them
  with a best-effort `chown` that warns and keeps the SSH user's ownership on
  EPERM. Mtimes are restored with nanosecond precision in every mode.
- **Receiver pipeline.** No per-file fsync by default (`--fsync` opts in);
  the root directory is synced once at the end (Linux). Files are applied
  with a bounded in-flight window so disk writes overlap; in-flight applies
  are joined before `DeleteRequest` and `CreateLinks`, so deletions see a
  settled tree and the empty-parent prune can never climb above the
  canonical root (the receiver's root is canonicalized once at construction).
- **Links are decided at scan time (`sync/linkpolicy.rs`, pure).** The
  default policy (no link flag) classifies every symlink against the
  canonical source root (spec §3.2): **internal** links are recreated with a
  rewritten DEST-relative target (0 bytes — the destination stays
  self-contained; on a Windows target they become `.lnk` shortcuts instead,
  since Windows cannot represent a POSIX symlink); **external file** links
  are dereferenced by default (their content is copied through the link);
  **external directory** and dangling links are skipped by default
  (warning; recorded in `Manifest.skipped`, so a `--delete` run never
  removes a previously created destination link — or its recursed subtree —
  for a still-present source link, protected by prefix match). The
  fine-grained `--literal-*-links` switches flip one class at a time to
  **literal** preservation (kept as a link with the source's exact target
  string): `--literal-internal-links` (no DEST-relative rewrite),
  `--literal-external-file-links` (no dereference; ignored when the
  destination is Windows), `--literal-external-dir-links` (no skip).
  **`--literal-links`** (implied by `-a`) is the macro: every symlink —
  internal, external file, external directory, dangling, even a self-loop —
  is recreated with its **literal target string**, no rewriting,
  dereferencing, or skipping (the scanner returns before classifying; the
  destination probe records the same literal, so the quick check converges).
  A Windows-source `.lnk` is then opaque data — its original bytes are copied
  as a regular file, never parsed and rebuilt as a shortcut — and a symlink
  on a Windows target still materializes as a `.lnk` (the object kind is the
  only cross-OS change; the target string stays literal, separator conversion
  only). **`--follow-links`** (rsync `-L`) is the opposite macro: every link
  is dereferenced — file targets become regular files, directory targets are
  recursed under the link's path (the `--follow-links` machinery: entries
  carry explicit `source_path`s outside the root, marked `dereferenced` so
  `--remove-source-files` never deletes them; a visited set of canonical
  directories cuts cycles), and a Windows-source `.lnk` is followed to its
  target. **`--skip-links`** (rsync `--no-l`) wins over everything: every
  link and shortcut is skipped entirely — not synced, not followed (recorded
  in `Manifest.skipped`, so `--delete` protects it; Windows-source `.lnk`
  file targets included). Precedence: `--skip-links` > `--follow-links` >
  `--literal-links` > the `--literal-*-links` granular switches > default
  (the CLI warns on contradicting combinations). Destination probes
  record links faithfully (never dereference), so a policy change converges
  on the next run. The scanner records
  `.lnk` shortcuts on a Windows source (magic sniff + the `lnk` crate):
  internal targets become `.lnk` entries on a Windows destination or
  symlinks on a Unix one; external targets are copied as opaque binaries.
  Destination probes recognize a materialized shortcut by content, keyed on
  the paths the source classifies as links — a Unix-source symlink becomes an
  *extensionless* `.lnk` on a Windows target, so the next run records it as
  the same link (quick-check skip) instead of re-creating it, while a data
  file whose body merely starts with the `.lnk` magic is never misclassified.
  The receiver restores a `.lnk`'s own mtime through the plain metadata path
  (it is a regular file, unlike a symlink's `AT_SYMLINK_NOFOLLOW`), and link
  creation is atomic: the object is built at a sibling temp and renamed over
  the old occupant, so a create failure never loses the previous file.
  The planner's link quick check compares the target string *and* — with
  times preserved — the link's own mtime (a source-link mtime drift re-creates
  the destination link so the time converges; `--checksum` and `--no-times`
  fall back to the target string alone).
  Hard links are detected by shared inode and are skipped on platforms
  without inodes (Windows); a source hard-link group is re-formed against any
  content-matched in-sync member, so a destination relationship broken
  externally (a member replaced by a standalone file) is restored by re-linking
  instead of degrading the member to a copy — a flag-induced skip
  (`--ignore-existing`, `--update`) never becomes a link target, since it can
  sit on divergent content. The sender batches links into a
  `Frame::CreateLinks` sent after all file content (hard link targets must
  exist); the receiver executes the `LinkSpec` (kind + already-rewritten
  target) without re-deciding. The receiver's path sanitizer still
  canonicalizes ancestors on every join (a containment check, not a link
  decision — kept per the spec review).
- **`--backup`** renames the replaced destination file to `<name>~` before
  the atomic install (rsync semantics; `--backup-dir`/`--suffix` are not
  implemented).
- **`--remove-source-files`** frees the source disk only after the data is
  **verified and durable** on the receiver: the receiver hashes each applied
  file while writing it (incremental BLAKE3, requested via `verify` in the
  `IndexRequest`) and returns the hashes in the `Ack`; the sender compares
  them against the hash it computed *while reading the source* — the delta
  checksum for delta/whole/batch transfers (already the BLAKE3 of the whole
  file; it is computed **only** when verification is requested — the default
  mode carries no whole-file checksum, `Delta.checksum` is `None`, and no
  whole-file hash runs on either side), an on-the-fly chunk hasher for large
  streamed files — so no re-read
  of the source. The delta checksum verification and this report share one
  hash pass (`apply_patch` takes the caller's hasher), so an applied file is
  hashed exactly once. Verification implies durability: the receiver fsyncs every
  file (`fsync || verify`) and syncs the renamed directories *before* the
  `Ack`, so a crash after the source is deleted cannot lose the destination.
  The receiver also re-stats each applied file (size+mtime vs the source
  metadata) to catch a post-apply modification, and the sender re-stats each
  source (size+mtime vs the scan) before deleting, so a same-size in-place
  change never loses data. Never deletes directories or symlinks, never a
  file the receiver skipped, and never a file whose size changed
  mid-transfer. On pull the flag reaches the server sender through the server
  argv — a pull with `--remove-source-files` deletes the *remote* source
  files, so the client forwards it only when the user actually set it and
  never as a side effect of `--verify` (the two flags are decoupled in
  `server_args`).
- **`-a/--archive`** is the byte-identical rsync archive bundle: the `rlpt`
  core is always on, and `-a` additionally recreates special files (fifos,
  sockets, block/char devices — `FileKind`/`rdev` on the wire), restores
  **owner/group** with a best-effort `chown` (a non-root receiver hits EPERM,
  warns, and keeps the SSH user's ownership), keeps **SUID/SGID/Sticky**
  (`& 0o7777`, protocol v17), and implies **`--literal-links`** — every link is
  recreated with its literal target (see the link-policy bullet),
  so a `-a` mirror is byte-identical down to the link strings. Nanosecond
  mtimes are restored in every mode.
  The **default** (no `-a`) keeps the 0-Root model: owner is the SSH
  connection user (no chown) and the high bits are cleared (`& 0o777`).
  Specials are contentless and travel in `CreateLinks`; `mkfifo` needs no
  privileges while device `mknod` is best-effort (a non-root receiver gets
  `EPERM`, warns, and skips — rsync behavior). These parts exist only on
  Unix-like systems: the Windows scanner records nothing and the receiver
  no-ops.
- **Permission bits follow the §2.2 matrix, computed on the source side.**
  Unix→Unix keeps `st_mode & 0o777` (SUID/SGID/Sticky force-cleared — the
  spec's `& 0o7777` would retain them; the stated intent wins), with
  `--no-perms` yielding explicit 0644/0755; Unix→Windows discards bits
  (NTFS ACLs are inherited — the receiver's mode apply is a no-op there);
  Windows→Unix uses the `exec_hint` heuristic (`.exe/.bat/.cmd/.ps1/.sh/
  .pl/.py/.rb/.lua` → 0755, else 0644, dirs 0755), disabled by
  `--no-perms`. The wire mode is the final value; the receiver applies it
  verbatim.
- **`-S/--sparse`** (rsync `-S`, protocol-agnostic) writes files sparsely on
  the receiver: a `SparseWriter` (wrapped around the staged file, *under* the
  verification hasher so every logical byte still feeds BLAKE3) splits each
  write at its non-zero bytes and seeks past zero runs ≥ 4096 bytes instead
  of writing them — a whole-file delta literal with a big interior hole
  arrives in one mixed buffer and still becomes a hole on disk. `finish()`
  flushes the sub-threshold pending zeros and truncates to the announced
  size, materializing a trailing hole; both write paths (delta recipe and
  chunked stream) use the same wrapper. `StagedFile::prepare` switches from
  `posix_fallocate` preallocation to a hole-extending `set_len` under `-S`
  (preallocation would allocate the blocks the sparse writer skips; the
  default path is unchanged).
- **`-X/--xattrs`** (rsync `-X`, protocol v18) copies extended attributes
  for files and directories: the scanner collects them in a post-pass
  (`listxattr`/`getxattr` per entry, best-effort — an unreadable attribute
  set contributes nothing), they ride as `FileMeta.xattrs` (`None` off the
  wire when the feature is off), and the receiver applies them via
  `setxattr` per name after the metadata apply — a name that cannot be set
  (a `security.*` attribute as a non-root receiver, a read-only filesystem)
  warns once and keeps the file. Symlinks are not covered; on Linux, POSIX
  ACLs ride along as opaque `system.posix_acl_*` attributes. Xattrs are not
  part of the quick check (an xattr-only change does not retrigger a file).
- **`-U/--atimes`** (rsync `-U`, protocol v18) restores the source's
  last-access time: `FileMeta.atime`/`atime_nsec` always ride the wire
  (captured free from the scan's `metadata`), and the receiver's
  `apply_times` sets `times[0]` from them — without `-U` it is `UTIME_OMIT`,
  leaving the receiver's atime alone (a small behavior change: the pre-v18
  code stamped atime = mtime). Independent of `--no-times` (each timespec
  slot is set or omitted independently); atime is never part of the quick
  check.
- **`--verify`** is the same on-the-fly BLAKE3 comparison without the deletion:
  it confirms the destination bytes match what the sender read (delta checksum
  or chunk hasher — no re-reads; the delta checksum exists only in this mode
  and under `--remove-source-files`, so the default path performs no
  whole-file hash on either side), reports any mismatch as a skipped file (exit
  23), and deletes nothing. It verifies only what *this run transferred*; a
  whole-tree re-hash of already-in-sync files is `--checksum`. Implies the
  receiver's per-file fsync (verified ⇒ durable). On pull the flag reaches the
  server sender through the server argv, independently of
  `--remove-source-files` — a verified-only pull never deletes anything.
- **`--max-delete N`** is a receiver-side safety valve: a `DeleteRequest`
  exceeding the limit aborts the sync before anything is removed.
- **`--existing` / `--ignore-times`** are planner flags (rsync semantics);
  on pull they reach the remote sender through the `PullRequest` frame.
  **`--ignore-existing`** is a transfer rule, not a delete rule (rsync
  semantics): any existing *file or symlink* at a name is left untouched —
  including a type change (file ↔ link), a content difference, or a drifted
  link target — while directories are still replaced ("rsync does not ignore
  existing directories"), and `--delete` still removes destination extras.
- **`-W/--watch` realtime sync** (`commands/watch.rs`). Push and local copy
  are event-driven (`notify` recursive watcher → debounce quiet window with a
  10s coalesce cap → incremental sync; changes during a sync trigger an
  immediate resync; failed syncs retry with backoff). The watcher starts
  *before* the initial sync, so changes landing while the initial manifest is
  built are captured and re-synced on the first loop iteration. `-W=DUR`
  (d/h/m/s combinable, default 24h) bounds the whole session, initial sync
  included, via a timeout that exits cleanly at the cap. Pull is
  **server-driven**
  (protocol v12): the server watches its own source tree and runs incremental
  cycles over one persistent session (`Executor::pull_watch` / `serve_watch_pull`),
  ending when the client disconnects (EOF probe); the client re-scans its
  local destination per cycle — cheap local metadata, no network polling.
  Local copies (one-shot `cp2 SRC DST` and `--watch`) push over a spawned
  `cp2 --server` child (pipes) rooted at the destination, so the delta engine
  and the full protocol semantics (symlinks, hard links, `--delete`, metadata)
  apply there too. The notify adapter lives in `sync/watcher.rs`,
  shared by the CLI loop and the server session; `Access` events are filtered
  so readers do not trigger syncs.
- **Storage-adaptive worker tuning.** The receiver detects the destination
  filesystem's class (`platform/storage`: Linux sysfs `queue/rotational`,
  Windows volume `IOCTL_STORAGE_QUERY_PROPERTY` seek-penalty, macOS
  `diskutil info` `Solid State`; `Unknown` elsewhere) and picks the apply
  window: 1 on a spinning disk (parallel writers thrash one head), 16 on
  SSD/NVMe. An explicit `-j/--jobs` always wins, and `--storage hdd|ssd`
  forces the class. `-j` reaches the remote side through the server argv
  (`spawn_ssh` appends `--jobs N`), so tuning is all-directional with no
  protocol change.
- **Adapted from studied crates, not linked.** copia (Delta/DeltaOp/apply_patch), sparsync
  (frame protocol, scan/manifest pipeline), sy (planner, transport abstraction),
  robosync (file-tier strategy), and pxs (parallel chunked single-file
  transfer, removed in the ssh-stdio refactor) informed the design.

## Public API (via the `cp2` library crate)

```rust
use cp2::{Executor, ExecutorOptions, Location};
use std::io::{AsyncRead, AsyncWrite};

// The executor runs the sync protocol over any byte stream — here the
// ssh-stdio channel spawns `ssh user@host cp2 --server`.
let send: Box<dyn AsyncWrite + Unpin + Send> = /* ssh child stdin */;
let recv: Box<dyn AsyncRead + Unpin + Send> = /* ssh child stdout */;

let mut executor = Executor::new(send, recv);
let stats = executor.push("/path/to/dir", &ExecutorOptions::default()).await?;
```

## CLI usage

Rsync-style: `cp2 SRC DST`, direction inferred from which side is remote
(`user@host:path`, port defaults to 22 — set it with `--port`, never in the
target string: a numeric suffix like `host:2222` is a path, not a port).

```bash
# push side (ssh drives auth: keys, agent, PAM password, ...).
# First run auto-deploys cp2 to ~/.cargo/bin/cp2 on the remote.
cp2 -p 2222 /path/to/dir "alice@127.0.0.1"

# pull side (`:backup` is relative to the account home)
cp2 -p 2222 "alice@127.0.0.1:backup" /path/to/restore

# absolute remote path (leading `/` after the colon)
cp2 /path/to/dir "alice@host:/home/alice/backup"

# local copy
cp2 ./src ./dst

# dry run
cp2 /path/to/dir user@host --dry-run
```

`cp2 --server` is the sshd-invoked server mode (not for direct use). The
client auto-deploys the matching binary to `~/.cargo/bin/cp2` on the remote
by default (`--remote-path` to change the location, `--no-auto-install` to
skip), so a fresh server needs no setup — sshd only has to accept the login.

Key flags: `-a/--archive` (full rsync `-a`: adds special files/devices on
Unix-like systems — the `rlpt` core is always on; `-a` also restores
owner/group (best-effort chown) and SUID/SGID/Sticky, and implies
`--literal-links` (literal link targets, `.lnk` copied as opaque files) —
byte-identical mode;
the default is 0-Root: permission bits only, owner is the SSH connection
user; nanosecond mtimes are restored in every mode),
`-p/--port`, `--remote-path`, `--binaries-dir`,
`--no-auto-install`,
`-n/--dry-run`, `-W/--watch`, `--watch-delay`, `--delete`, `--max-delete`,
`-u/--update`,
`-c/--checksum`, `--ignore-existing`, `--existing`, `--ignore-times`,
`--backup`, `--remove-source-files`, `--verify`, `--exclude/--include`, `-j/--jobs`,
`-z/--compress`, `--bwlimit`, `--fsync`, `-v/--verbose`,
`-S/--sparse`, `-X/--xattrs`, `-U/--atimes` (all opt-in, rsync-parity —
`-a` deliberately does not imply them),
`--literal-links`/`--follow-links`/`--skip-links` and the granular
`--literal-internal-links`/`--literal-external-file-links`/
`--literal-external-dir-links`, plus the `rlpt`
opt-outs `--no-recursive`/`--skip-links`/`--no-perms`/`--no-times`. See
`README.md` for the full table.
Defaults mirror `rsync -avP` (minus `-z`): recursive with mode/mtime
preservation, a per-file listing annotated with the file's position and the
run's total (`[12/3456] path`), terminal progress showing the percentage,
the live transfer speed, and the remaining count, and keep-partials on
abort. `-q` silences the listing and progress.
The `rlpt` basics are always on; `--no-recursive`/`--skip-links`/`--no-perms`/
`--no-times` opt out of each (rsync `--no-r`/`--no-l`/`--no-p`/`--no-t`
semantics: `--skip-links` skips every symlink entirely and overrides
`--literal-links`/`--follow-links` and the granular switches, while
`--follow-links` overrides the literal family,
`--no-times` also falls back to the size-only quick check).

## Conventions

- `cargo test` runs lib + integration tests; keep the e2e push and pull tests
  passing (`tests/e2e_{transfer,links,meta,features}.rs` share the helpers in
  `tests/common/mod.rs` and run the protocol against a spawned `cp2 --server`
  child over pipes — no sshd needed in CI).
- The benchmarks live in `bench/` (sourced from `bench/lib.sh`):
  `bench/mixed-tree.sh` (≈10 GiB / 100 K-file fresh/second/edit + integrity,
  cp2 vs rsync) and `bench/single-file.sh` (single-file/small-tree delta
  scenarios, `MODE=large|small`); `bench/compare_test.sh` is the older
  cross-tool (cp2/rsync/scp/sy) run. All take `CP2_BIN`/`HOST`/`WORK` env.
- `cargo clippy` must be clean.
- Delta types are `serde`-serialized over the wire via `postcard`.
- The platform layer (`src/platform/`) is portable and dependency-free
  (`fs.rs` + `staging.rs` + `storage.rs`). There is no per-OS auth or storage
  code: authentication and access control are sshd's, so nothing here is
  OS-specific (`storage.rs` carries a Linux sysfs probe, a Windows
  volume-IOCTL probe, and a macOS `diskutil` probe for HDD/SSD detection;
  other platforms report `Unknown` and callers fall back).
- `~/.cargo/registry` is always readable for reference source of the studied
  crates (copia, sparsync, sy, robosync, etc.).
