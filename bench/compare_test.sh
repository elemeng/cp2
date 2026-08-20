#!/usr/bin/env bash
# compare_test.sh — cp2 vs scp vs rsync vs sy over the push SSH path.
#
# Scenarios (all pushes over ssh):
#   large-first  <LARGE_TOTAL_MB> (two files) fresh push
#   large-edit   overwrite 1 MiB in the middle of one large file, push again
#   small-first  the many-small-files tree fresh push
#   small-idle   push again with no changes (quick-check overhead)
#
# The small-files tree is generated (8192 files, 1-64 KiB, 64 subdirs,
# ~256 MiB) unless --small-src points at a real tree (e.g. the repo's .git
# or target/ — pick one large enough that the transfer time is not drowned
# in the ssh-setup noise: a 4 MiB .git is too small to measure).
#
# Usage: bench/compare_test.sh [--remote user@host] [--small-src DIR]
#                              [--large-mb 1024]
# Env overrides: REMOTE, SMALL_SRC, LARGE_TOTAL_MB, REMOTE_BASE
# (REMOTE_BASE is created under the remote account home; the remote side
# needs a writable home with room for ~4x the small tree + large files).

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
REMOTE="${REMOTE:-user@localhost}"
REMOTE_BASE="${REMOTE_BASE:-cp2-bench}"
SMALL_SRC="${SMALL_SRC:-}"          # empty -> generate a synthetic tree
LARGE_TOTAL_MB="${LARGE_TOTAL_MB:-1024}"
SMALL_FILES="${SMALL_FILES:-8192}"
WORK="${WORK:-$(mktemp -d "$HOME/.cache/cp2-bench.XXXXXX")}"
TOOLS=(cp2 rsync scp sy)

if [ ! -x "$REPO/target/release/cp2" ]; then
    cargo build --release --manifest-path "$REPO/Cargo.toml"
fi

echo "== compare_test: push over ssh to $REMOTE =="

# Clean the remote bench root.
ssh "$REMOTE" "rm -rf ~/$REMOTE_BASE && mkdir -p ~/$REMOTE_BASE"

# Large sources: one pristine copy per tool (the edit scenario mutates it).
mkdir -p "$WORK/large"
head -c $((LARGE_TOTAL_MB / 2))M /dev/urandom > "$WORK/large/data1.bin"
head -c $((LARGE_TOTAL_MB / 2))M /dev/urandom > "$WORK/large/data2.bin"
echo "large        : ${LARGE_TOTAL_MB} MiB in 2 files"

# Small sources: a real tree when given, else a generated one (8192 files,
# 1-64 KiB, 64 subdirs — a reproducible "many small files" shape).
if [ -z "$SMALL_SRC" ]; then
    SMALL_SRC="$WORK/small-src"
    mkdir -p "$SMALL_SRC"
    i=0
    while [ "$i" -lt "$SMALL_FILES" ]; do
        d="d$(printf '%02d' $((i / 128)))"
        mkdir -p "$SMALL_SRC/$d"
        # Deterministic sizes in 1-64 KiB (Knuth multiplicative hashing).
        sz=$(( (i * 2654435761 % 64512) + 1024 ))
        head -c "$sz" /dev/urandom > "$SMALL_SRC/$d/f$i.bin"
        i=$((i + 1))
    done
fi
echo "small source : $SMALL_SRC ($(du -sh "$SMALL_SRC" | cut -f1), $(find "$SMALL_SRC" -type f | wc -l) files)"

# timeit <name> <cmd...> — wall seconds of the command, echoed on stdout.
# The command's own output goes to a per-tool log (progress lines would
# interleave with the timing report).
timeit() {
    local log="$WORK/$1.log"
    shift
    local t0 t1
    t0=$(date +%s.%N)
    "$@" > "$log" 2>&1
    t1=$(date +%s.%N)
    awk "BEGIN{printf \"%.2f\", $t1 - $t0}"
}

push_impl() { # tool src remote_dest
    local tool="$1" src="$2" rd="$3"
    case "$tool" in
        cp2)   "$REPO/target/release/cp2" "$src" "$REMOTE:$rd" ;;
        rsync) rsync -rlt "$src/" "$REMOTE:$rd/" ;;
        scp)   scp -r -q "$src/." "$REMOTE:$rd/" ;;
        sy)    sy "$src" "$REMOTE:$rd" ;;
    esac
}

# Warm the page cache for a source tree: the first tool to read a file pays
# the cold-cache cost, which would bias the comparison.
warm() {
    find "$1" -type f -exec cat {} + > /dev/null 2>&1 || true
}

declare -A RESULTS

for tool in "${TOOLS[@]}"; do
    large="$WORK/$tool-large"
    cp -r "$WORK/large" "$large"
    ssh "$REMOTE" "mkdir -p ~/$REMOTE_BASE/$tool/large ~/$REMOTE_BASE/$tool/small"
    warm "$large"
    warm "$SMALL_SRC"

    RESULTS["$tool large-first"]=$(timeit "$tool-large-first" push_impl "$tool" "$large" "$REMOTE_BASE/$tool/large")

    # Overwrite 1 MiB in the middle of the first large file (delta update).
    dd if=/dev/urandom of="$large/data1.bin" bs=1M seek=$((LARGE_TOTAL_MB / 4)) count=1 conv=notrunc status=none
    RESULTS["$tool large-edit"]=$(timeit "$tool-large-edit" push_impl "$tool" "$large" "$REMOTE_BASE/$tool/large")

    RESULTS["$tool small-first"]=$(timeit "$tool-small-first" push_impl "$tool" "$SMALL_SRC" "$REMOTE_BASE/$tool/small")

    RESULTS["$tool small-idle"]=$(timeit "$tool-small-idle" push_impl "$tool" "$SMALL_SRC" "$REMOTE_BASE/$tool/small")
done

# Report.
printf "\n%-8s %12s %12s %12s %12s\n" tool large-first large-edit small-first small-idle
for tool in "${TOOLS[@]}"; do
    printf "%-8s %11ss %11ss %11ss %11ss\n" \
        "$tool" \
        "${RESULTS[$tool large-first]}" \
        "${RESULTS[$tool large-edit]}" \
        "${RESULTS[$tool small-first]}" \
        "${RESULTS[$tool small-idle]}"
done

printf "\nnotes: small-idle for scp re-copies everything (no delta/quick-check).\n"
printf "       work dir: %s\n" "$WORK"
