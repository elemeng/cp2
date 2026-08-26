#!/usr/bin/env bash
# compare_studied.sh — the SSH-capable studied crates, run through the same
# four-scenario harness as compare_test.sh (identical trees, warm cache,
# per-tool logs, single host).
#
# Participants (from the README acknowledgments): sy and pxs — the only
# studied crates that actually sync over ssh. msy installs the `sy`
# binary itself (same lineage, already on the list); sparsync needs a
# serve/enroll/auth mesh; syncz wraps the system rsync; zsync-rs is an HTTP
# delta client; robosync and rusync do not parse `user@host:path` (they copy
# into a literal local directory of that name); copia is a library. cp2
# and rsync are the reference rows, measured in the same run.
#
# Usage: bench/compare_studied.sh [--remote user@host] [--small-src DIR]
#                                 [--large-mb 1024]
# Env overrides: REMOTE, SMALL_SRC, LARGE_TOTAL_MB, REMOTE_BASE

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
REMOTE="${REMOTE:-user@localhost}"
REMOTE_BASE="${REMOTE_BASE:-cp2-bench-studied}"
SMALL_SRC="${SMALL_SRC:-}"          # empty -> generate a synthetic tree
LARGE_TOTAL_MB="${LARGE_TOTAL_MB:-1024}"
SMALL_FILES="${SMALL_FILES:-8192}"
WORK="${WORK:-$(mktemp -d "$HOME/.cache/cp2-bench-studied.XXXXXX")}"
# A studied tool that hangs (or a stale absolute path) must not stall the
# whole harness: every run is bounded and its rc recorded.
RUN_TIMEOUT="${RUN_TIMEOUT:-300}"
read -r -a TOOLS <<< "${TOOLS:-cp2 rsync scp sy pxs}"
# The exported `push_impl` runs in a `bash -c` child (bounded by `timeout`),
# which does not see plain shell variables — the ones the dispatcher reads
# must cross the process boundary explicitly.
export REPO REMOTE REMOTE_BASE

if [ ! -x "$REPO/target/release/cp2" ]; then
    cargo build --release --manifest-path "$REPO/Cargo.toml"
fi

echo "== compare_studied: push over ssh to $REMOTE =="
for t in sy pxs scp; do
    command -v "$t" >/dev/null || { echo "missing tool: $t (cargo install $t)" >&2; exit 1; }
done

ssh "$REMOTE" "rm -rf ~/$REMOTE_BASE && mkdir -p ~/$REMOTE_BASE"

# Large sources: one pristine copy per tool (the edit scenario mutates it).
mkdir -p "$WORK/large"
head -c $((LARGE_TOTAL_MB / 2))M /dev/urandom > "$WORK/large/data1.bin"
head -c $((LARGE_TOTAL_MB / 2))M /dev/urandom > "$WORK/large/data2.bin"
echo "large        : ${LARGE_TOTAL_MB} MiB in 2 files"

if [ -z "$SMALL_SRC" ]; then
    SMALL_SRC="$WORK/small-src"
    mkdir -p "$SMALL_SRC"
    i=0
    while [ "$i" -lt "$SMALL_FILES" ]; do
        d="d$(printf '%02d' $((i / 128)))"
        mkdir -p "$SMALL_SRC/$d"
        sz=$(( (i * 2654435761 % 64512) + 1024 ))
        head -c "$sz" /dev/urandom > "$SMALL_SRC/$d/f$i.bin"
        i=$((i + 1))
    done
fi
echo "small source : $SMALL_SRC ($(du -sh "$SMALL_SRC" | cut -f1), $(find "$SMALL_SRC" -type f | wc -l) files)"

timeit() { # name tool src rd — runs push_impl for the tool, bounded
    local name="$1"; shift
    local t0 t1 rc
    t0=$(date +%s.%N)
    # `if` (not a bare command) keeps `set -e` from aborting the harness on a
    # tool failure — the rc is recorded, the run continues.
    if timeout "$RUN_TIMEOUT" bash -c 'push_impl "$@"' _ "$@" > "$WORK/$name.log" 2>&1; then
        rc=0
    else
        rc=$?
    fi
    t1=$(date +%s.%N)
    echo "$rc" > "$WORK/$name.rc"
    awk "BEGIN{printf \"%.2f\", $t1 - $t0}" > "$WORK/$name.t"
}

push_impl() { # tool src remote_dest
    local tool="$1" src="$2" rd="$3"
    case "$tool" in
        cp2)     "$REPO/target/release/cp2" "$src" "$REMOTE:$rd" ;;
        rsync)   rsync -rlt "$src/" "$REMOTE:$rd/" ;;
        sy)      sy "$src" "$REMOTE:$rd" ;;
        pxs)     pxs sync "$src" "$REMOTE:$rd" ;;
        scp)     scp -r -q "$src/." "$REMOTE:$rd/" ;;
    esac
}

# export the dispatcher (now that it exists) so the bounded subprocess can
# call it from `bash -c 'push_impl "$@"'`.
export -f push_impl

warm() {
    find "$1" -type f -exec cat {} + > /dev/null 2>&1 || true
}

declare -A RESULTS
declare -A RCS

for tool in "${TOOLS[@]}"; do
    large="$WORK/$tool-large"
    cp -r "$WORK/large" "$large"
    ssh "$REMOTE" "mkdir -p ~/$REMOTE_BASE/$tool/large ~/$REMOTE_BASE/$tool/small"
    warm "$large"
    warm "$SMALL_SRC"

    timeit "$tool-large-first" "$tool" "$large" "$REMOTE_BASE/$tool/large"
    RESULTS["$tool large-first"]=$(cat "$WORK/$tool-large-first.t")
    RCS["$tool large-first"]=$(cat "$WORK/$tool-large-first.rc")

    dd if=/dev/urandom of="$large/data1.bin" bs=1M seek=$((LARGE_TOTAL_MB / 4)) count=1 conv=notrunc status=none
    timeit "$tool-large-edit" "$tool" "$large" "$REMOTE_BASE/$tool/large"
    RESULTS["$tool large-edit"]=$(cat "$WORK/$tool-large-edit.t")
    RCS["$tool large-edit"]=$(cat "$WORK/$tool-large-edit.rc")

    timeit "$tool-small-first" "$tool" "$SMALL_SRC" "$REMOTE_BASE/$tool/small"
    RESULTS["$tool small-first"]=$(cat "$WORK/$tool-small-first.t")
    RCS["$tool small-first"]=$(cat "$WORK/$tool-small-first.rc")

    timeit "$tool-small-idle" "$tool" "$SMALL_SRC" "$REMOTE_BASE/$tool/small"
    RESULTS["$tool small-idle"]=$(cat "$WORK/$tool-small-idle.t")
    RCS["$tool small-idle"]=$(cat "$WORK/$tool-small-idle.rc")
done

printf "\n%-8s %12s %12s %12s %12s\n" tool large-first large-edit small-first small-idle
for tool in "${TOOLS[@]}"; do
    printf "%-8s %11ss %11ss %11ss %11ss" \
        "$tool" \
        "${RESULTS[$tool large-first]}" \
        "${RESULTS[$tool large-edit]}" \
        "${RESULTS[$tool small-first]}" \
        "${RESULTS[$tool small-idle]}"
    bad=""
    for s in large-first large-edit small-first small-idle; do
        [ "${RCS[$tool $s]}" != "0" ] && bad="$bad $s(rc=${RCS[$tool $s]})"
    done
    [ -n "$bad" ] && printf "  rc!=0:$bad"
    printf "\n"
done

printf "\nnotes: 1 MiB pipe endpoints, page cache warmed before each tool; every run\n"
printf "       bounded by a %ss timeout with rc recorded. Remote scratch under\n" "$RUN_TIMEOUT"
printf "       ~/%s on $REMOTE; work dir: %s\n" "$REMOTE_BASE" "$WORK"