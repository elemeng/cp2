#!/usr/bin/env bash
# Real-ssh smoke test for cp2: runs push/pull over an actual sshd with the
# system-ssh transport (the ControlMaster mux, auto-deploy, the stderr
# drain), verifying byte integrity at every step. Covers the paths the
# pipe-based integration tests cannot: ssh spawn, remote deploy, the mux,
# and server-side stderr flowing back.
#
# Usage: scripts/smoke-ssh.sh [host] [cp2-binary]
#   host        ssh target host (default localhost; key auth required,
#               same account; a user@host form also works)
#   cp2-binary  the client binary to test (default target/release/cp2)
#
# Needs: bash, ssh key auth to the host, and a release build (or the binary
# passed explicitly). The remote cp2 is deployed to ~/.cache/cp2-smoke-bin
# and the working tree to ~/.cache/cp2-smoke-<rand>; both are removed on
# exit.
set -euo pipefail

HOST="${1:-localhost}"
CP2="${2:-$(cd "$(dirname "$0")/.." && pwd)/target/release/cp2}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/cp2-smoke.XXXXXX")"
REMOTE_ROOT=".cache/cp2-smoke-$(basename "$WORK")"
REMOTE_PATH=".cache/cp2-smoke-bin/cp2"
TIMEOUT=120
FAIL=0

if [[ "$HOST" != *@* ]]; then
    TARGET="$(whoami)@$HOST"
else
    TARGET="$HOST"
fi

note()  { printf '\033[1;34m== %s\033[0m\n' "$*"; }
pass()  { printf '\033[1;32mok:  %s\033[0m\n' "$*"; }
fail()  { printf '\033[1;31mFAIL: %s\033[0m\n' "$*"; FAIL=1; }

cleanup() {
    rm -rf "$WORK"
    ssh -o BatchMode=yes -o ConnectTimeout=10 "$TARGET" "rm -rf $REMOTE_ROOT .cache/cp2-smoke-bin" 2>/dev/null || true
}
trap cleanup EXIT

if [[ ! -x "$CP2" ]]; then
    echo "cp2 binary not found: $CP2 (build it first: cargo build --release)" >&2
    exit 2
fi
if ! ssh -o BatchMode=yes -o ConnectTimeout=10 "$TARGET" true 2>/dev/null; then
    echo "ssh key auth to $TARGET failed — the smoke test needs a passwordless key" >&2
    exit 2
fi

# A fixed tree: 300 small files, 8 dirs, one 8 MiB file (delta candidate),
# plus a symlink. Deterministic content so the second push is a no-op.
note "build source tree"
SEED_DIR="$WORK/src"
mkdir -p "$SEED_DIR"/d{1..8}
for f in "$SEED_DIR"/d{1..8}/f{1..300}.txt; do
    printf 'payload %s\n' "$(basename "$(dirname "$f")")/$(basename "$f")" > "$f"
done
head -c 8388608 /dev/urandom > "$SEED_DIR/large.bin"
ln -s d1/f1.txt "$SEED_DIR/link"
ln -s ../large.bin "$SEED_DIR/d2/link2"
rm -rf "$WORK/dst"; mkdir -p "$WORK/dst"

SSH="ssh -o BatchMode=yes $TARGET"

note "push (deploy + first sync)"
# Source trailing slash (rsync semantics): the tree lands directly under
# the destination, not under a recreated last component.
if timeout "$TIMEOUT" "$CP2" -a --remote-path "$REMOTE_PATH" "$SEED_DIR/" "$TARGET:$REMOTE_ROOT/dst" >"$WORK/push1.log" 2>&1; then
    pass "push rc=0"
else
    fail "push failed"; tail -5 "$WORK/push1.log" >&2
fi
count=$($SSH "find $REMOTE_ROOT/dst -type f 2>/dev/null | wc -l")
[[ "$count" -eq 2401 ]] && pass "2401 files on the remote ($count)" || fail "file count: $count != 2401"

note "no-op re-sync must find nothing to send"
# Source trailing slash (rsync semantics): the tree lands directly under
# the destination, not under a recreated last component.
if timeout "$TIMEOUT" "$CP2" -a --remote-path "$REMOTE_PATH" "$SEED_DIR/" "$TARGET:$REMOTE_ROOT/dst" >"$WORK/push2.log" 2>&1; then
    grep -q "0 files, 0 bytes" "$WORK/push2.log" && pass "no-op re-sync" || fail "no-op sent bytes"
else
    fail "no-op re-sync failed"; tail -3 "$WORK/push2.log" >&2
fi

note "edit + delta re-sync"
dd if=/dev/urandom of="$SEED_DIR/large.bin" bs=1M seek=2 count=1 conv=notrunc status=none
echo new >> "$SEED_DIR/d3/fnew.txt"
# Source trailing slash (rsync semantics): the tree lands directly under
# the destination, not under a recreated last component.
if timeout "$TIMEOUT" "$CP2" -a --remote-path "$REMOTE_PATH" "$SEED_DIR/" "$TARGET:$REMOTE_ROOT/dst" >"$WORK/push3.log" 2>&1; then
    pass "edit push rc=0"
else
    fail "edit push failed"; tail -5 "$WORK/push3.log" >&2
fi
# The changed 1 MiB must have crossed the delta, byte-for-byte.
local_sum=$(sha256sum "$SEED_DIR/large.bin" | cut -d' ' -f1)
remote_sum=$($SSH "sha256sum $REMOTE_ROOT/dst/large.bin" 2>/dev/null | cut -d' ' -f1 || true)
if [[ "$local_sum" == "$remote_sum" ]]; then
    pass "remote large.bin matches the edited source ($local_sum)"
else
    fail "remote large.bin differs: local $local_sum remote $remote_sum"
    grep -E "plan:|large" "$WORK/push3.log" >&2 || true
fi

note "pull (restore) + byte comparison"
if timeout "$TIMEOUT" "$CP2" -a --remote-path "$REMOTE_PATH" "$TARGET:$REMOTE_ROOT/dst" "$WORK/restore" >"$WORK/pull.log" 2>&1; then
    pass "pull rc=0"
else
    fail "pull failed"; tail -5 "$WORK/pull.log" >&2
fi
# The remote source has no trailing slash, so the `dst` component is
# recreated under the local destination (rsync semantics).
if diff -r "$SEED_DIR" "$WORK/restore/dst" >/dev/null 2>&1; then
    pass "restore is byte-identical to the source"
else
    fail "restore differs from the source"; diff -rq "$SEED_DIR" "$WORK/restore/dst" | head -5 >&2
fi

note "verbose run: server stderr must flow back without stalling"
if timeout "$TIMEOUT" "$CP2" --remote-path "$REMOTE_PATH" -v "$SEED_DIR/" "$TARGET:$REMOTE_ROOT/dst2" >"$WORK/verbose.log" 2>&1; then
    grep -q "Synced" "$WORK/verbose.log" && pass "server summary reached the client" || fail "no server summary in output"
else
    fail "verbose run failed"; tail -3 "$WORK/verbose.log" >&2
fi

echo
if [[ "$FAIL" -eq 0 ]]; then
    echo "SMOKE PASS"
else
    echo "SMOKE FAIL" >&2
    exit 1
fi