#!/usr/bin/env bash
# bench.sh — the one benchmark comparison harness. Everything the old
# bench/ scripts did (compare_test.sh, compare_studied.sh, compare_remote.sh,
# mixed-tree.sh, single-file.sh + lib.sh) lives here as a suite:
#
#   bench.sh compare [tool ...]   four-scenario cross-tool table
#                                 (large-first / large-edit / small-first /
#                                 small-idle), cp2 rsync scp sy pxs by
#                                 default; one run each, rc recorded
#   bench.sh mixed [tool ...]    the ≈10 GiB / 100 K-file tree (70 K small
#                                 1-16 KiB, 27 K medium, 3 K large 1-2 MiB),
#                                 default cp2 vs rsync: fresh / second /
#                                 edit / integrity — the same tree is
#                                 selectable in the other suites (MIXED=1 on
#                                 compare/remote, MODE=mixed on single)
#   bench.sh single               the delta-focused runs (MODE=large|small:
#                                 fresh / edit A+B / insert / idle, cp2 vs
#                                 rsync)
#   bench.sh remote               daily-flow comparison over a real network
#                                 (cp2 vs rsync: fresh / idle / edit,
#                                 RUNS-averaged, MiB/s from each tool's own
#                                 transferred-volume summary)
#
# REMOTE selects the ssh target — whoami@localhost by default, a real host
# for true-network measurements:  REMOTE=user@host bench.sh compare
#
# Env (all optional):
#   CP2_BIN             cp2 binary (default: this repo's release build)
#   REMOTE              ssh target, user@host (default: whoami@localhost)
#   REMOTE_BASE         remote scratch dir under the account home
#   WORK                local scratch dir (default: a fresh mktemp, removed
#                       unless KEEP_WORK=1)
#   LARGE_TOTAL_MB      compare suite: large files total MiB (default 1024)
#   SMALL_FILES         generated small-tree file count (default 8192)
#   SMALL_SRC           compare suite: use a real tree instead of generating
#   TOOLS               ignored — tools are positional after the suite name
#   RUN_TIMEOUT         per-run bound in seconds (default 300)
#   MODE / FILE_MB      single suite: large|small tree / large file MiB
#   RUNS / EDIT_FILES   remote suite: repetitions / files mutated per edit
#   BINARIES_DIR        cp2 sidecar dir for the deploy
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
REMOTE="${REMOTE:-$(whoami)@localhost}"
REMOTE_BASE="${REMOTE_BASE:-cp2-bench}"
CP2_BIN="${CP2_BIN:-$REPO/target/release/cp2}"
WORK="${WORK:-$(mktemp -d "${TMPDIR:-$HOME/.cache}/cp2-bench.XXXXXX")}"
KEEP_WORK="${KEEP_WORK:-0}"
RUN_TIMEOUT="${RUN_TIMEOUT:-300}"
LARGE_TOTAL_MB="${LARGE_TOTAL_MB:-1024}"
SMALL_FILES="${SMALL_FILES:-8192}"
SMALL_SRC="${SMALL_SRC:-}"
# The mixed tree (≈10 GiB / 100 K files) is an optional workload for every
# suite: MIXED=1 on compare/remote, MODE=mixed on single. Its buckets can be
# resized with SMALL_FILES/MEDIUM_FILES/LARGE_FILES (defaults below).
MIXED="${MIXED:-0}"
MIX_SMALL="${MIX_SMALL:-70000}" MIX_MEDIUM="${MIX_MEDIUM:-27000}" MIX_LARGE="${MIX_LARGE:-3000}"

[ -x "$CP2_BIN" ] || { echo "cp2 binary missing at $CP2_BIN — run 'cargo build --release' first" >&2; exit 1; }
command -v rsync >/dev/null || { echo "rsync not on PATH" >&2; exit 1; }
command -v awk >/dev/null || { echo "awk not on PATH" >&2; exit 1; }

cleanup() {
    [ "$KEEP_WORK" = 1 ] || rm -rf "$WORK"
    ssh "$REMOTE" "rm -rf ~/$REMOTE_BASE/$$" 2>/dev/null || true
}
trap cleanup EXIT

# Time one command, bounded by RUN_TIMEOUT, rc recorded into $WORK/NAME.rc,
# output captured into $WORK/NAME.log — echoes the wall seconds.
# usage: timeit NAME -- cmd...
timeit() {
    local name="$1"; shift
    shift # the "--"
    local t0 t1 rc
    t0=$(date +%s.%N)
    # `if` (not a bare command) keeps `set -e` from aborting on a tool
    # failure — the rc is recorded, the harness continues.
    if timeout "$RUN_TIMEOUT" "$@" >"$WORK/$name.log" 2>&1; then rc=0; else rc=$?; fi
    t1=$(date +%s.%N)
    echo "$rc" > "$WORK/$name.rc"
    awk "BEGIN{printf \"%.2f\", $t1 - $t0}"
}

# Time one command, print an aligned "name  time  rc  note" line.
# usage: run NAME -- cmd...   (the `--` separator is the caller's)
run() {
    local name="$1"; shift
    local t rc note
    t=$(timeit "$name" "$@")
    rc=$(cat "$WORK/$name.rc")
    note=$(grep -oE "Done: [0-9]+ files, [0-9]+ bytes transferred|sent [0-9,]+ bytes|Synced [0-9]+ files" "$WORK/$name.log" | tail -1) || note=
    printf "%-18s %9.2fs   rc=%d   %s\n" "$name" "$t" "$rc" "$note"
}

# The per-tool push for the compare suite (exported: it runs in the timeout
# child via `bash -c`).
push_impl() { # tool src remote_dest_rel
    local tool="$1" src="$2" rd="$3"
    case "$tool" in
        cp2)     "$CP2_BIN" "$src" "$REMOTE:$rd" ;;
        rsync)   rsync -rlt "$src/" "$REMOTE:$rd/" ;;
        scp)     scp -r -q "$src/." "$REMOTE:$rd/" ;;
        sy)      sy "$src" "$REMOTE:$rd" ;;
        pxs)     pxs sync "$src" "$REMOTE:$rd" ;;
    esac
}
export -f push_impl
# The timeout child (`bash -c`) does not see plain shell variables.
export REMOTE REMOTE_BASE CP2_BIN

# Warm the page cache for a source tree: the first tool to read a file pays
# the cold-cache cost, which would bias the comparison.
warm() { find "$1" -type f -exec cat {} + > /dev/null 2>&1 || true; }

# Generate the small-files tree (8192 files, 1-64 KiB, 64 subdirs — a
# reproducible "many small files" shape) unless SMALL_SRC points at a real
# tree (pick one large enough that the transfer time is not drowned in the
# ssh-setup noise).
gen_small_tree() { # dest
    local dst="$1" i=0 sz d
    mkdir -p "$dst"
    while [ "$i" -lt "$SMALL_FILES" ]; do
        d="d$(printf '%02d' $((i / 128)))"
        mkdir -p "$dst/$d"
        # Deterministic sizes in 1-64 KiB (Knuth multiplicative hashing).
        sz=$(( (i * 2654435761 % 64512) + 1024 ))
        head -c "$sz" /dev/urandom > "$dst/$d/f$i.bin"
        i=$((i + 1))
    done
}

# The two 0.5*LARGE_TOTAL_MB files (the edit scenario mutates a per-tool
# copy, so each tool starts from the same pristine content).
gen_large_files() { # dest
    local dst="$1"
    mkdir -p "$dst"
    head -c $((LARGE_TOTAL_MB / 2))M /dev/urandom > "$dst/data1.bin"
    head -c $((LARGE_TOTAL_MB / 2))M /dev/urandom > "$dst/data2.bin"
}

echo "== bench $* — target $REMOTE (work $WORK) =="

# ---------------------------------------------------------------------------
cmd_compare() { # [tool ...] — the four-scenario cross-tool table
    local -a TOOLS=("$@")
    [ "${#TOOLS[@]}" -gt 0 ] || TOOLS=(cp2 rsync scp sy pxs)
    for t in "${TOOLS[@]}"; do
        case "$t" in
            cp2)   : ;;
            rsync) command -v rsync >/dev/null || { echo "missing tool: rsync" >&2; exit 1; } ;;
            scp)   command -v scp >/dev/null || { echo "missing tool: scp" >&2; exit 1; } ;;
            sy|pxs) command -v "$t" >/dev/null || { echo "missing tool: $t (cargo install $t)" >&2; exit 1; } ;;
            *) echo "unknown tool: $t (cp2 rsync scp sy pxs)" >&2; exit 1 ;;
        esac
    done
    if [ "$MIXED" = 1 ]; then
        cmd_mixed "${TOOLS[@]}"
        return
    fi

    if [ -z "$SMALL_SRC" ]; then
        SMALL_SRC="$WORK/small-src"
        gen_small_tree "$SMALL_SRC"
    fi
    gen_large_files "$WORK/large"
    echo "large: ${LARGE_TOTAL_MB} MiB in 2 files; small: $SMALL_SRC ($(du -sh "$SMALL_SRC" | cut -f1), $(find "$SMALL_SRC" -type f | wc -l) files)"

    ssh "$REMOTE" "mkdir -p ~/$REMOTE_BASE/$$/compare"
    local -A RESULTS RCS
    local tool s t
    for tool in "${TOOLS[@]}"; do
        local large="$WORK/$tool-large"
        cp -r "$WORK/large" "$large"
        ssh "$REMOTE" "mkdir -p ~/$REMOTE_BASE/$$/compare/$tool/large ~/$REMOTE_BASE/$$/compare/$tool/small"
        warm "$large"
        warm "$SMALL_SRC"

        t=$(timeit "$tool-large-first" -- bash -c 'push_impl "$@"' _ "$tool" "$large" "$REMOTE_BASE/$$/compare/$tool/large")
        RESULTS["$tool large-first"]=$t
        RCS["$tool large-first"]=$(cat "$WORK/$tool-large-first.rc")

        # Overwrite 1 MiB mid-file (delta update).
        dd if=/dev/urandom of="$large/data1.bin" bs=1M seek=$((LARGE_TOTAL_MB / 4)) count=1 conv=notrunc status=none
        t=$(timeit "$tool-large-edit" -- bash -c 'push_impl "$@"' _ "$tool" "$large" "$REMOTE_BASE/$$/compare/$tool/large")
        RESULTS["$tool large-edit"]=$t
        RCS["$tool large-edit"]=$(cat "$WORK/$tool-large-edit.rc")

        t=$(timeit "$tool-small-first" -- bash -c 'push_impl "$@"' _ "$tool" "$SMALL_SRC" "$REMOTE_BASE/$$/compare/$tool/small")
        RESULTS["$tool small-first"]=$t
        RCS["$tool small-first"]=$(cat "$WORK/$tool-small-first.rc")

        t=$(timeit "$tool-small-idle" -- bash -c 'push_impl "$@"' _ "$tool" "$SMALL_SRC" "$REMOTE_BASE/$$/compare/$tool/small")
        RESULTS["$tool small-idle"]=$t
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
        local bad=""
        for s in large-first large-edit small-first small-idle; do
            [ "${RCS[$tool $s]}" != "0" ] && bad="$bad $s(rc=${RCS[$tool $s]})"
        done
        [ -n "$bad" ] && printf "  rc!=0:$bad"
        printf "\n"
    done
    echo
    echo "notes: page cache warmed before each tool; every run bounded by a ${RUN_TIMEOUT}s"
    echo "       timeout with rc recorded; small-idle for scp re-copies everything."
}

# ---------------------------------------------------------------------------
# The mixed tree: ≈10 GiB / 100 K files (70 K small 1-16 KiB, 27 K medium
# 64-384 KiB, 3 K large 1-2 MiB), sized by MIX_SMALL/MIX_MEDIUM/MIX_LARGE.
# Shared by the `mixed` suite and the MIXED=1 / MODE=mixed options of the
# other suites.

gen_mixed_tree() { # dest
    command -v python3 >/dev/null || { echo "the mixed tree needs python3" >&2; exit 1; }
    local dest="$1"
    mkdir -p "$dest"
    echo "== generating mixed tree ($((MIX_SMALL + MIX_MEDIUM + MIX_LARGE)) files, ~2 MiB avg) =="
    python3 - "$dest" "$MIX_SMALL" "$MIX_MEDIUM" "$MIX_LARGE" <<'EOF'
import os, random, sys
root, n_s, n_m, n_l = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
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
}

# The edit phase for the mixed tree: 1 K appends + 0.8 K in-place rewrites +
# 200 new + 100 deleted (deterministic seed). Mutates `root` in place — the
# fresh/second phases must run before it (or on a copy).
mutate_mixed_tree() { # root
    python3 - "$1" "$MIX_SMALL" "$MIX_MEDIUM" "$MIX_LARGE" <<'EOF'
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
    p = path(i)
    os.makedirs(os.path.dirname(p), exist_ok=True)   # new dirs on resized trees
    with open(p, "wb") as f:
        f.write(os.urandom(rng.randint(1024, 100000)))
for i in rng.sample(range(1, n_s + n_m + n_l + 1), min(100, n_s)):
    os.remove(path(i))
EOF
}

# ---------------------------------------------------------------------------
cmd_mixed() { # [tool ...] — the mixed-tree phases: fresh / second / edit /
              # integrity, one destination per tool. The default tools are
              # cp2 and rsync; `bench.sh compare MIXED=1 [tool ...]` and
              # `bench.sh single MODE=mixed` run the same phases.
    local -a MTOOLS=("$@")
    [ "${#MTOOLS[@]}" -gt 0 ] || MTOOLS=(cp2 rsync)
    local SRC="$WORK/mixed-src" RW="$WORK/.rw" tool

    gen_mixed_tree "$SRC"
    mkdir -p "$RW"
    cp -al "$SRC"/. "$RW/"      # hardlink copies: mutate $RW, keep $SRC pristine

    for tool in "${MTOOLS[@]}"; do
        [ "$tool" = cp2 ] || [ "$tool" = rsync ] || command -v "$tool" >/dev/null \
            || { echo "missing tool: $tool" >&2; exit 1; }
    done

    local RD="$REMOTE_BASE/$$/mixed/dst"

    echo "== fresh =="
    for tool in "${MTOOLS[@]}"; do
        ssh "$REMOTE" "mkdir -p ~/$RD/$tool"
        case "$tool" in
            cp2)   run "$tool fresh" -- "$CP2_BIN" "$SRC" "$REMOTE:$RD/$tool" ;;
            rsync) run "$tool fresh" -- rsync -rpt "$SRC/" "$REMOTE:$RD/$tool/" ;;
            *)     run "$tool fresh" -- bash -c 'push_impl "$@"' _ "$tool" "$SRC" "$RD/$tool" ;;
        esac
    done

    echo "== second =="
    for tool in "${MTOOLS[@]}"; do
        case "$tool" in
            cp2)   run "$tool second" -- "$CP2_BIN" "$SRC" "$REMOTE:$RD/$tool" ;;
            rsync) run "$tool second" -- rsync -rpt "$SRC/" "$REMOTE:$RD/$tool/" ;;
            *)     run "$tool second" -- bash -c 'push_impl "$@"' _ "$tool" "$SRC" "$RD/$tool" ;;
        esac
    done

    echo "== edit =="
    mutate_mixed_tree "$RW"
    for tool in "${MTOOLS[@]}"; do
        case "$tool" in
            cp2)   run "$tool edit" -- "$CP2_BIN" "$RW" "$REMOTE:$RD/$tool" ;;
            rsync) run "$tool edit" -- rsync -rpt "$RW/" "$REMOTE:$RD/$tool/" ;;
            *)     run "$tool edit" -- bash -c 'push_impl "$@"' _ "$tool" "$RW" "$RD/$tool" ;;
        esac
    done

    echo "== integrity =="
    # rsync's checksum dry-run reads both sides and lists only files that
    # would be transferred (the source's deliberately-deleted files are not
    # listed without --delete): 0 differing = byte-identical.
    for tool in "${MTOOLS[@]}"; do
        local diffs
        diffs=$(rsync -rltc --dry-run "$RW/" "$REMOTE:$RD/$tool/" 2>/dev/null | grep -cE '^[-A-Za-z].*\.bin$' || true)
        echo "$tool integrity: $diffs differing files"
    done
}

# ---------------------------------------------------------------------------
cmd_single() { # MODE=large|small|mixed — delta scenarios, cp2 vs rsync
    local MODE="${MODE:-large}"
    local FILE_MB="${FILE_MB:-1024}"
    local SRC="$WORK/src" i d sz NDIRS

    mkdir -p "$SRC"
    if [ "$MODE" = large ]; then
        head -c "$((FILE_MB * 1048576))" /dev/urandom > "$SRC/data.bin"
    elif [ "$MODE" = mixed ]; then
        gen_mixed_tree "$SRC"
    else
        NDIRS=$((SMALL_FILES / 128))
        for i in $(seq 0 $((SMALL_FILES - 1))); do
            d="d$(printf '%02d' $((i / 128)))"
            mkdir -p "$SRC/$d"
            sz=$(( (i * 2654435761 % 64512) + 1024 ))
            head -c "$sz" /dev/urandom > "$SRC/$d/f$i.bin"
        done
    fi
    echo "mode=$MODE src=$(du -sh "$SRC" | cut -f1) files=$(find "$SRC" -type f | wc -l)"

    local RD="$REMOTE_BASE/$$/single/dst"
    ssh "$REMOTE" "mkdir -p ~/$RD/cp2 ~/$RD/rsync"

    # Warm the local page cache between scenarios; the remote side is warmed
    # by reading back the destination file (both tools re-read their basis).
    warm_remote() { ssh "$REMOTE" "cat ~/$1 > /dev/null" 2>/dev/null || true; }

    timeit2() { # name src — cp2 then rsync, one row (both bounded)
        local name="$1" src="$2" t0 t1 tc tr
        t0=$(date +%s.%N)
        timeout "$RUN_TIMEOUT" "$CP2_BIN" "$src" "$REMOTE:$RD/cp2" > "$WORK/cp2.$name.log" 2>&1 || true
        t1=$(date +%s.%N)
        tc=$(awk "BEGIN{printf \"%.3f\", $t1 - $t0}")
        t0=$(date +%s.%N)
        timeout "$RUN_TIMEOUT" rsync -rlt "$src/" "$REMOTE:$RD/rsync/" > "$WORK/rsync.$name.log" 2>&1 || true
        t1=$(date +%s.%N)
        tr=$(awk "BEGIN{printf \"%.3f\", $t1 - $t0}")
        printf "%-10s cp2 %8.3fs   rsync %8.3fs\n" "$name" "$tc" "$tr"
    }
    run_scenario() { # name
        warm "$SRC"
        if [ "$MODE" = large ]; then
            warm_remote "$RD/cp2/data.bin"
        else
            warm_remote "$RD/cp2/d00/f0.bin"
        fi
        timeit2 "$1" "$SRC"
    }

    run_scenario fresh1
    run_scenario fresh2
    if [ "$MODE" = mixed ]; then
        mutate_mixed_tree "$SRC"
        for i in 1 2 3; do run_scenario "edit-$i"; done
    elif [ "$MODE" = large ]; then
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
        for d in $(seq 0 $((SMALL_FILES / 128 - 1))); do
            dd if=/dev/urandom of="$SRC/d$(printf '%02d' $d)/f$((d * 128)).bin" bs=1K count=1 conv=notrunc status=none
        done
        for i in 1 2 3; do run_scenario "editA-$i"; done
        for d in $(seq 0 $((SMALL_FILES / 128 - 1))); do
            dd if=/dev/urandom of="$SRC/d$(printf '%02d' $d)/f$((d * 128 + 1)).bin" bs=1K count=1 conv=notrunc status=none
        done
        for i in 1 2 3; do run_scenario "editB-$i"; done
    fi
    for i in 1 2; do run_scenario "idle$i"; done
}

# ---------------------------------------------------------------------------
cmd_remote() { # daily-flow cp2 vs rsync over a real network: fresh/idle/edit
    local RUNS="${RUNS:-2}" EDIT_FILES="${EDIT_FILES:-32}"
    local DEST_CP2="$REMOTE_BASE/$$/remote/cp2" DEST_RSYNC="$REMOTE_BASE/$$/remote/rsync"
    local SRC="${SRC:-$REPO/target}"
    if [ "$MIXED" = 1 ]; then
        SRC="$WORK/mixed-src"
        gen_mixed_tree "$SRC"
    fi

    # A non-local remote may run an older glibc than this build's — the
    # deploy must push the statically linked musl sidecar or the remote
    # binary cannot load.
    if [ "$REMOTE" != "$(whoami)@localhost" ] \
        && [ ! -f "$(dirname "$CP2_BIN")/cp2-x86_64-unknown-linux-musl" ] \
        && [ -z "${BINARIES_DIR:-}" ]; then
        echo "warning: no cp2-x86_64-unknown-linux-musl next to $CP2_BIN" >&2
        echo "         the deploy may push the glibc build and fail on the remote" >&2
        echo "         (GLIBC_2.xx not found); build and place it with:" >&2
        echo "         cargo build --release --target x86_64-unknown-linux-musl" >&2
        echo "         cp target/x86_64-unknown-linux-musl/release/cp2 $(dirname "$CP2_BIN")/" >&2
    fi

    local CP2_ARGS=()
    [ -n "${BINARIES_DIR:-}" ] && CP2_ARGS+=(--binaries-dir "$BINARIES_DIR")

    echo "== cp2 vs rsync push: $REMOTE (src $SRC, $(du -sh "$SRC" | cut -f1), $(find "$SRC" -type f | wc -l) files)"
    ssh "$REMOTE" "rm -rf ~/$DEST_CP2 ~/$DEST_RSYNC && mkdir -p ~/$DEST_CP2 ~/$DEST_RSYNC"

    run_cp2() { "$CP2_BIN" "${CP2_ARGS[@]}" "$SRC" "$REMOTE:$DEST_CP2"; }
    run_rsync() { rsync -rlt --stats "$SRC/" "$REMOTE:$DEST_RSYNC/"; }
    cp2_bytes() { grep -o '[0-9]* bytes transferred' "$WORK/remote.log" | grep -o '[0-9]*' | tail -1 || echo 0; }
    rsync_bytes() { sed -n 's/.*transferred file size: *\([0-9,]*\) bytes.*/\1/p' "$WORK/remote.log" | tr -d ',' | tail -1 || echo 0; }
    mutate_edit_files() { # rewrite EDIT_FILES files in place (content + mtime)
        find "$SRC" -type f | sort | head -"$EDIT_FILES" | while read -r f; do
            dd if=/dev/urandom of="$f" bs=1K count=1 conv=notrunc status=none 2>/dev/null || true
        done || true
    }
    fmt() { # time bytes -> aligned "time  MiB  MiB/s" line
        local t="$1" b="${2:-0}"
        printf "%9ss" "$t"
        if [ -n "$b" ] && [ "$b" -gt 0 ] 2>/dev/null; then
            awk "BEGIN{printf \"  %10.1f MiB  %8.1f MiB/s\", $b/1048576, $b/1048576/$t}"
        else
            printf "  %10s  %9s" "-" "-"
        fi
        echo
    }
    one_run() { # cp2|rsync -> wall seconds (log captured for the bytes fn)
        local t0 t1
        t0=$(date +%s.%N)
        # Inline (not an exported function): the runner needs the locals.
        if [ "$1" = cp2 ]; then
            timeout "$RUN_TIMEOUT" "$CP2_BIN" "${CP2_ARGS[@]}" "$SRC" "$REMOTE:$DEST_CP2" \
                > "$WORK/remote.log" 2>&1 || true
        else
            timeout "$RUN_TIMEOUT" rsync -rlt --stats "$SRC/" "$REMOTE:$DEST_RSYNC/" \
                > "$WORK/remote.log" 2>&1 || true
        fi
        t1=$(date +%s.%N)
        awk "BEGIN{printf \"%.3f\", $t1 - $t0}"
    }

    printf "%-9s %-16s %11s  %12s  %10s\n" tool scenario time bytes speed
    for tool in cp2 rsync; do
        local acc_t acc_b
        local bytes_fn
        [ "$tool" = cp2 ] && bytes_fn=cp2_bytes || bytes_fn=rsync_bytes

        # fresh: one full transfer (cp2's deploy included when stale)
        printf "%-9s %-16s" "$tool" "fresh"
        fmt "$(one_run "$tool")" "$($bytes_fn)"

        # idle: no changes — the per-run overhead
        acc_t=0
        for _ in $(seq 1 "$RUNS"); do
            acc_t=$(awk "BEGIN{print $acc_t + $(one_run "$tool")}")
        done
        printf "%-9s %-16s" "$tool" "idle"
        fmt "$(awk "BEGIN{printf \"%.3f\", $acc_t / $RUNS}")"

        # edit: mutate before every run — each run is a real incremental
        acc_t=0; acc_b=0
        for _ in $(seq 1 "$RUNS"); do
            mutate_edit_files
            acc_t=$(awk "BEGIN{print $acc_t + $(one_run "$tool")}")
            acc_b=$(awk "BEGIN{print $acc_b + $($bytes_fn)}")
        done
        printf "%-9s %-16s" "$tool" "edit ($EDIT_FILES)"
        fmt "$(awk "BEGIN{printf \"%.3f\", $acc_t / $RUNS}")" "$(awk "BEGIN{printf \"%.0f\", $acc_b / $RUNS}")"
    done

    echo
    echo "notes:"
    echo "  - bytes = the logical volume each tool materialized (comparable; on a delta"
    echo "    both re-write the touched files in full)."
    echo "  - idle transfers nothing (quick check) — its time is the per-run overhead."
    echo "  - the first fresh cp2 run includes the auto-deploy when the remote binary is stale."
}

# ---------------------------------------------------------------------------
suite="${1:-compare}"
shift || true
case "$suite" in
    compare) cmd_compare "$@" ;;
    mixed)   cmd_mixed "$@" ;;
    single)  cmd_single ;;
    remote)  cmd_remote ;;
    *) echo "unknown suite: $suite (compare | mixed | single | remote)" >&2; exit 1 ;;
esac
echo "work dir: $WORK (KEEP_WORK=1 to retain)"