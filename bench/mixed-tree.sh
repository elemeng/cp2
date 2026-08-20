#!/usr/bin/env bash
# Mixed-tree benchmark: ≈10 GiB / 100 K files (70 K small 1-16 KiB, 27 K
# medium 64-384 KiB, 3 K large 1-2 MiB), cp2 vs rsync over ssh.
#
# Phases (rsync semantics per phase, unchanged sources):
#   fresh   full transfer        not latency-bound — throughput comparison
#   second  quick check, no op   per-run overhead
#   edit    1 K appends + 0.8 K in-place rewrites + 200 new + 100 deleted
#   integrity  byte-compare both destinations against the (edited) source
#
# Requires: python3, rsync, bc, and the cp2 release build (see lib.sh).
#
# Env: SMALL_FILES / MEDIUM_FILES / LARGE_FILES to resize the tree (the
# byte profile is fixed per bucket; total ≈ 1.9 MB per file on average).
set -euo pipefail
source "$(dirname "$0")/lib.sh"

SRC="$WORK/src"
SMALL_FILES="${SMALL_FILES:-70000}"
MEDIUM_FILES="${MEDIUM_FILES:-27000}"
LARGE_FILES="${LARGE_FILES:-3000}"
RW="${WORK}/.rw"   # the edit phase only mutates these hardlink copies

echo "== generating tree ($((SMALL_FILES + MEDIUM_FILES + LARGE_FILES)) files, ~2 MiB avg) =="
mkdir -p "$SRC"
python3 - "$SRC" "$SMALL_FILES" "$MEDIUM_FILES" "$LARGE_FILES" <<'EOF'
import os, random, sys
root, n_s, n_m, n_l = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
# Buckets are interleaved across directories so the plan order mixes sizes;
# small ends at 16 KiB, medium at 384 KiB, large at 2 MiB.
rng = random.Random(2026)
def write(rel, size):
    p = os.path.join(root, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "wb") as f:
        f.write(os.urandom(size))
for i in range(1, n_s + 1):
    write(f"d{i % 100 + 1}/f{i}.bin", rng.randint(1, 16 * 1024))
for i in range(n_s + 1, n_s + n_m + 1):
    write(f"d{i % 100 + 1}/f{i}.bin", rng.randint(64 * 1024, 384 * 1024))
for i in range(n_s + n_m + 1, n_s + n_m + n_l + 1):
    write(f"d{i % 100 + 1}/f{i}.bin", rng.randint(1024 * 1024, 2 * 1024 * 1024))
EOF
mkdir -p "$RW"
cp -al "$SRC"/. "$RW/"      # hardlink copies: mutate $RW, keep $SRC pristine

# Remote scratch (under the remote home: no /tmp assumptions on the server).
RD="cp2-bench/$$/dst"
ensure_remote_dirs "$RD/cp2" "$RD/rsync"
trap 'ssh "$HOST" "rm -rf cp2-bench/'$$'" 2>/dev/null || true' EXIT

echo "== fresh =="
run "cp2 fresh"   "$CP2_BIN" "$SRC" "$HOST:$RD/cp2"
run "rsync fresh" rsync -rpt "$SRC/" "$HOST:$RD/rsync/"

echo "== second =="
run "cp2 second"   "$CP2_BIN" "$SRC" "$HOST:$RD/cp2"
run "rsync second" rsync -rpt "$SRC/" "$HOST:$RD/rsync/"

echo "== edit =="
python3 - "$RW" "$SMALL_FILES" "$MEDIUM_FILES" "$LARGE_FILES" <<'EOF'
import os, random, sys
root, n_s, n_m, n_l = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
rng = random.Random(7)
def path(i):
    return f"{root}/d{i % 100 + 1}/f{i}.bin"
for i in rng.sample(range(1, n_s + 1), min(1000, n_s)):
    with open(path(i), "ab") as f:
        f.write(b"x" * rng.randint(1, 64))
for i in rng.sample(range(n_s + 1, n_s + n_m + 1), min(500, n_m)):
    with open(path(i), "r+b") as f:
        f.seek(rng.randint(0, 300000)); f.write(os.urandom(16384))
for i in rng.sample(range(n_s + n_m + 1, n_s + n_m + n_l + 1), min(300, n_l)):
    with open(path(i), "r+b") as f:
        f.seek(rng.randint(0, 1500000)); f.write(os.urandom(262144))
for i in range(n_s + n_m + n_l + 1, n_s + n_m + n_l + 201):
    with open(path(i), "wb") as f:
        f.write(os.urandom(rng.randint(1024, 100000)))
for i in rng.sample(range(1, n_s + n_m + n_l + 1), min(100, n_s)):
    os.remove(path(i))
EOF
run "cp2 edit"   "$CP2_BIN" "$RW" "$HOST:$RD/cp2"
run "rsync edit" rsync -rpt "$RW/" "$HOST:$RD/rsync/"

echo "== integrity =="
# rsync's checksum dry-run reads both sides and lists only files that would
# be transferred (the source's deliberately-deleted files are not listed
# without --delete): an empty list = byte-identical.
rc1=$(rsync -rltc --dry-run "$RW/" "$HOST:$RD/cp2/" 2>/dev/null | grep -cE '^[-A-Za-z].*\.bin$' || true)
rc2=$(rsync -rltc --dry-run "$RW/" "$HOST:$RD/rsync/" 2>/dev/null | grep -cE '^[-A-Za-z].*\.bin$' || true)
echo "cp2 integrity:   $rc1 differing files"
echo "rsync integrity: $rc2 differing files"
echo
cat "$WORK/summary.txt"