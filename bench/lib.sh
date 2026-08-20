#!/usr/bin/env bash
# Shared helpers for the bench/ scenario scripts (mixed-tree.sh,
# single-file.sh): cross-tool timing, scratch management, and integrity
# checks. Source this, do not execute.
#
# Environment (all optional):
#   CP2_BIN     path to the cp2 binary (default: the repo's release build)
#   HOST        ssh target for the remote side, user@host (default: whoami@localhost)
#   WORK        scratch dir (default: a fresh tempdir, removed on exit)
#   KEEP_WORK=1 keep the scratch dir after the run
set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CP2_BIN="${CP2_BIN:-$REPO/target/release/cp2}"
HOST="${HOST:-$(whoami)@localhost}"
WORK="${WORK:-$(mktemp -d "${TMPDIR:-/tmp}/cp2-bench.XXXXXX")}"
KEEP_WORK="${KEEP_WORK:-0}"

[ -x "$CP2_BIN" ] || { echo "cp2 binary missing at $CP2_BIN — run 'cargo build --release' first" >&2; exit 1; }
command -v rsync >/dev/null || { echo "rsync not on PATH" >&2; exit 1; }

: > "$WORK/summary.txt"

# Time one tool run, record name / wall seconds / rc / summary line. Safe
# under `set -e`: a failing tool is recorded, not fatal.
# usage: run NAME -- cmd...
run() {
    local name="$1"; shift
    local out="$WORK/$name.log"
    local start end rc
    start=$(date +%s.%N)
    if "$@" >"$out" 2>&1; then rc=0; else rc=$?; fi
    end=$(date +%s.%N)
    local t
    t=$(echo "$end - $start" | bc)
    local note
    note=$(grep -oE "Done: [0-9]+ files, [0-9]+ bytes transferred|sent [0-9,]+ bytes|Synced [0-9]+ files" "$out" | tail -1) || note=
    printf "%-18s %9.2fs   rc=%d   %s\n" "$name" "$t" "$rc" "$note" | tee -a "$WORK/summary.txt"
}

# Ensure the destination directories exist on the remote side (a scoped
# scratch dir namespaced under the remote home so no `mkdir -p` of absolute
# /tmp paths is needed).
ensure_remote_dirs() { # path1 path2 ...
    ssh "$HOST" "mkdir -p $*" || { echo "cannot reach $HOST" >&2; exit 1; }
}

# Where the remote copies land (cleared by the scenario scripts).
dst_base() { echo "$WORK/dst"; }

cleanup() { [ "$KEEP_WORK" = 1 ] || rm -rf "$WORK"; }
trap cleanup EXIT