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
  destination storage class (1 on HDD, 8 on SSD/NVMe — detected via sysfs
  / IOCTL / `diskutil`); an explicit `-j` always wins.
- Hashing is SIMD-accelerated (blake3/rayon).

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
