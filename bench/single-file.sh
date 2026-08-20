#!/usr/bin/env bash
# Single-file / small-tree delta benchmark: cp2 vs rsync over ssh, with the
# per-run scenarios that show the delta engine's value (small rewrites and
# an insertion in a large file — both tools re-read the basis, so the wall
# time compares the quick-check + delta machinery, not the byte volume).
#
# MODE=large (default): one 1 GiB file; scenarios fresh, edit A/B (10 MiB
# overwritten at 512/256 MiB), insert (10 MiB @ 768 MiB), idle.
# MODE=small: 8192 files 1-64 KiB in 64 dirs; scenarios fresh, edit A/B
# (1 KiB rewritten in every 128th / 129th file), idle.
#
# Each edited scenario runs 3 times (same mutation, warm cache); the table
# prints per-scenario wall seconds for both tools.
#
# Env: MODE, FILE_MB (large-mode size), SMALL_FILES (small-mode count).
set -euo pipefail
source "$(dirname "$0")/lib.sh"

MODE="${MODE:-large}"
FILE_MB="${FILE_MB:-1024}"
SMALL_FILES="${SMALL_FILES:-8192}"

SRC="$WORK/src"
mkdir -p "$SRC"

if [ "$MODE" = large ]; then
    # /dev/urandom: the benchmark compares wall time to push the bytes, and
    # neither tool compresses by default — content is irrelevant.
    head -c "$((FILE_MB * 1048576))" /dev/urandom > "$SRC/data.bin"
    SCENARIOS=(fresh1 fresh2 editA-1 editA-2 editA-3 editB-1 editB-2 editB-3 insert-1 insert-2 insert-3 idle1 idle2)
else
    NDIRS=$((SMALL_FILES / 128))
    for i in $(seq 0 $((SMALL_FILES - 1))); do
        d="d$(printf '%02d' $((i / 128)))"
        mkdir -p "$SRC/$d"
        sz=$(( (i * 2654435761 % 64512) + 1024 ))
        head -c "$sz" /dev/urandom > "$SRC/$d/f$i.bin"
    done
    SCENARIOS=(fresh1 fresh2 editA-1 editA-2 editA-3 editB-1 editB-2 editB-3 idle1 idle2)
fi
echo "mode=$MODE src=$(du -sh "$SRC" | cut -f1) files=$(find "$SRC" -type f | wc -l)"

# Remote destinations (absolute paths — cp2/rsync handle them verbatim).
RD="$WORK/dst"
ssh "$HOST" "mkdir -p '$RD/cp2' '$RD/rsync'" || { echo "cannot reach $HOST" >&2; exit 1; }

# Warm the local page cache between scenarios; the remote side is warmed by
# reading back the destination file (both tools re-read their basis anyway).
warm() { find "$1" -type f -exec cat {} + > /dev/null 2>&1 || true; }
warm_remote() { ssh "$HOST" "cat '$1/$2' > /dev/null" 2>/dev/null || true; }

timeit() { # name src
    local name="$1" src="$2"
    local t0 t1 tc tr
    t0=$(date +%s.%N)
    "$CP2_BIN" "$src" "$HOST:$RD/cp2" > "$WORK/cp2.$name.log" 2>&1 || true
    t1=$(date +%s.%N)
    tc=$(echo "$t1 - $t0" | bc)
    t0=$(date +%s.%N)
    rsync -rlt "$src/" "$HOST:$RD/rsync/" > "$WORK/rsync.$name.log" 2>&1 || true
    t1=$(date +%s.%N)
    tr=$(echo "$t1 - $t0" | bc)
    printf "%-10s cp2 %8.3fs   rsync %8.3fs\n" "$name" "$tc" "$tr"
}

run_scenario() { # name (the source is already mutated where applicable)
    warm "$SRC"
    [ "$MODE" = large ] && warm_remote "$RD" data.bin || warm_remote "$RD" d00/f0.bin
    timeit "$1" "$SRC"
}

run_scenario fresh1
run_scenario fresh2

if [ "$MODE" = large ]; then
    dd if=/dev/urandom of="$SRC/data.bin" bs=1M seek=512 count=10 conv=notrunc status=none
    for i in 1 2 3; do run_scenario "editA-$i"; done
    dd if=/dev/urandom of="$SRC/data.bin" bs=1M seek=256 count=10 conv=notrunc status=none
    for i in 1 2 3; do run_scenario "editB-$i"; done
    head -c $((768 * 1048576)) "$SRC/data.bin" > "$WORK/tmp"
    head -c 10M /dev/urandom >> "$WORK/tmp"
    tail -c +$((768 * 1048576 + 1)) "$SRC/data.bin" >> "$WORK/tmp"
    mv "$WORK/tmp" "$SRC/data.bin"
    for i in 1 2 3; do run_scenario "insert-$i"; done
else
    for d in $(seq 0 $((NDIRS - 1))); do
        dd if=/dev/urandom of="$SRC/d$(printf '%02d' $d)/f$((d * 128)).bin" bs=1K count=1 conv=notrunc status=none
    done
    for i in 1 2 3; do run_scenario "editA-$i"; done
    for d in $(seq 0 $((NDIRS - 1))); do
        dd if=/dev/urandom of="$SRC/d$(printf '%02d' $d)/f$((d * 128 + 1)).bin" bs=1K count=1 conv=notrunc status=none
    done
    for i in 1 2 3; do run_scenario "editB-$i"; done
fi
for i in 1 2; do run_scenario "idle$i"; done

echo
echo "work: $WORK"