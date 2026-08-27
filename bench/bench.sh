#!/usr/bin/env bash
# bench.sh — the cp2 benchmark suite.
#
# MODEL
#   A bench is TOOLS x SCENARIOS over generated trees. Every cell is
#   measured by one engine: RUNS repetitions (default 3) of a bounded,
#   page-cache-warmed push, reported as MEAN ± SD (sample standard
#   deviation), with the worst rc and the mean materialized volume.
#   Integrity is a final checksum dry-run per tool. No single lucky run:
#   variance is visible, timeouts and failures are recorded, never hidden.
#
#   Scenario semantics per repetition (this is what makes a mean honest):
#     fresh   a fresh destination per run    — every run is a full transfer
#     second  one destination, unchanged     — every run is the no-op scan
#     edit    one destination, source        — every run is a real delta
#             re-mutated before each run
#
#   Control variables (comparisons stay fair): identical generated trees
#   per tool, isolated per-tool destinations, page-cache warm (or uniformly
#   cold with WARM=0), the tool order ROTATES per scenario so
#   time-correlated drift cannot favor a measurement slot, WARMUP
#   repetitions (default 0) discard one-time setup from the statistics,
#   and every run is bounded and rc-recorded.
#
#   Every suite runs on localhost or over a real network — REMOTE selects
#   the ssh target (whoami@localhost by default, user@host for real runs).
#
# SUITES (thin scenario lists over the engine)
#   bench.sh compare [tool ...]   default cp2 rsync scp sy pxs:
#                                 large-first / large-edit / small-first /
#                                 small-idle, integrity, fastest row
#                                 MIXED=1: the ≈10 GiB / 100 K-file phase
#                                 table (fresh / second / edit / integrity)
#   bench.sh single               delta scenarios, cp2 vs rsync:
#                                 MODE=large|small|mixed
#   bench.sh daily                the daily-flow perspective, cp2 vs rsync:
#                                 fresh / idle / edit with throughput
#                                 (MiB/s from each tool's own volume)
#
# EXTENDING
#   tool:      one case in push_impl (and volume_of for the byte summary)
#   tree:      one gen_* function
#   scenario:  one measure() call in the suite
#
# ENV (all optional)
#   CP2_BIN             cp2 binary (default: this repo's release build)
#   REMOTE              ssh target, user@host (default: whoami@localhost)
#   REMOTE_BASE         remote scratch dir under the account home
#   WORK                local scratch dir (default: a fresh mktemp, removed
#                       unless KEEP_WORK=1)
#   RUNS                repetitions per cell kept in the statistics
#                       (default 3; 1 = fastest)
#   WARMUP              extra first repetitions per cell, discarded from
#                       the statistics (default 0; 1+ absorbs one-time
#                       setup — the deploy probe, master creation — so
#                       the cells measure steady state)
#   WARM                1 = pre-warm the page cache before each run
#                       (default 1); 0 = cold-cache runs
#   JSON                1 = additionally print a machine-readable record
#                       set per cell (mean, sd, min, max, runs, rc, bytes)
#   RUN_TIMEOUT         per-run bound in seconds (default 300)
#   LARGE_TOTAL_MB      compare: large files total MiB (default 1024)
#   SMALL_FILES         generated small-tree file count (default 8192)
#   SMALL_SRC           compare: use a real tree instead of generating
#   MIX_SMALL/MIX_MEDIUM/MIX_LARGE   mixed-tree bucket sizes
#   MODE / FILE_MB      single: large|small|mixed / large file MiB
#   EDIT_FILES          remote: files mutated per edit run
#   BINARIES_DIR        cp2 sidecar dir for the deploy
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
REMOTE="${REMOTE:-$(whoami)@localhost}"
REMOTE_BASE="${REMOTE_BASE:-cp2-bench}"
CP2_BIN="${CP2_BIN:-$REPO/target/release/cp2}"
WORK="${WORK:-$(mktemp -d "${TMPDIR:-$HOME/.cache}/cp2-bench.XXXXXX")}"
KEEP_WORK="${KEEP_WORK:-0}"
RUNS="${RUNS:-3}"
WARMUP="${WARMUP:-0}"
WARM="${WARM:-1}"
JSON="${JSON:-0}"
RUN_TIMEOUT="${RUN_TIMEOUT:-300}"
LARGE_TOTAL_MB="${LARGE_TOTAL_MB:-1024}"
SMALL_FILES="${SMALL_FILES:-8192}"
SMALL_SRC="${SMALL_SRC:-}"
MIX_SMALL="${MIX_SMALL:-70000}" MIX_MEDIUM="${MIX_MEDIUM:-27000}" MIX_LARGE="${MIX_LARGE:-3000}"
MIXED="${MIXED:-0}"

[ -x "$CP2_BIN" ] || { echo "cp2 binary missing at $CP2_BIN — run 'cargo build --release' first" >&2; exit 1; }
command -v rsync >/dev/null || { echo "rsync not on PATH" >&2; exit 1; }
command -v awk >/dev/null || { echo "awk not on PATH" >&2; exit 1; }

cleanup() {
    [ "$KEEP_WORK" = 1 ] || rm -rf "$WORK"
    ssh "$REMOTE" "rm -rf ~/$REMOTE_BASE/$$" 2>/dev/null || true
}
trap cleanup EXIT

# ============================================================================
# ENGINE
# ============================================================================

# The per-tool push (exported: it runs in the timeout child via bash -c).
push_impl() { # tool src remote_dest_rel
    local tool="$1" src="$2" rd="$3"
    case "$tool" in
        cp2)   "$CP2_BIN" "$src" "$REMOTE:$rd" ;;
        # -a (no -c): the same default core cp2 runs — recursive, links,
        # perms, times, owner/group — with the same size+mtime quick check.
        # --stats feeds the volume extractor.
        rsync) rsync -a --stats "$src/" "$REMOTE:$rd/" ;;
        scp)   scp -r -q "$src/." "$REMOTE:$rd/" ;;
        sy)    sy "$src" "$REMOTE:$rd" ;;
        pxs)   pxs sync "$src" "$REMOTE:$rd" ;;
    esac
}
export -f push_impl
# The timeout child (bash -c) does not see plain shell variables.
export REMOTE REMOTE_BASE CP2_BIN

# Materialized volume a tool's log reports (bytes); 0 when the tool prints
# no summary (scp/sy/pxs — those cells then show "-").
volume_of() { # tool logfile -> bytes materialized (0 when no summary)
    local v=0
    case "$1" in
        cp2)
            v=$(grep -o '[0-9]* bytes transferred' "$2" 2>/dev/null | grep -o '[0-9]*' | tail -1) || v=0 ;;
        rsync)
            v=$(sed -n 's/.*transferred file size: *\([0-9,]*\) bytes.*/\1/p' "$2" 2>/dev/null | tr -d ',' | tail -1) || v=0 ;;
    esac
    [ -n "$v" ] || v=0
    echo "$v"
}

# Warm the page cache for a source tree: the first tool to read a file pays
# the cold-cache cost, which would bias the comparison.
warm() { find "$1" -type f -exec cat {} + > /dev/null 2>&1 || true; }

# The measurement core: RUNS bounded pushes of one (tool, scenario) cell.
#   measure LABEL TOOL SRC RD [MUTATE_FN] [PER_RUN_DEST]
#     MUTATE_FN    a function re-mutating SRC before every run (edit cells)
#     PER_RUN_DEST=1  a fresh "$RD/r$i" destination per run (fresh cells)
#   Prints "mean sd rc bytes" (sample SD over the run times; rc = worst).
measure() {
    local label="$1" tool="$2" src="$3" rd="$4" mutate="${5:-}" per_run="${6:-0}"
    local i j t rc b t0 t1 dest times="" rcs="" bytes=""
    local -a tarr rarr barr
    for i in $(seq 1 $((RUNS + WARMUP))); do
        [ -n "$mutate" ] && "$mutate"
        [ "$WARM" = 1 ] && warm "$src"
        dest="$rd"; [ "$per_run" = 1 ] && dest="$rd/r$i"
        t0=$(date +%s.%N)
        # `if` (not a bare command) keeps `set -e` from aborting on a tool
        # failure — the rc is recorded, the harness continues.
        if timeout "$RUN_TIMEOUT" bash -c 'push_impl "$@"' _ "$tool" "$src" "$dest" >"$WORK/$label.$i.log" 2>&1; then
            rc=0
        else
            rc=$?
        fi
        t1=$(date +%s.%N)
        t=$(awk "BEGIN{printf \"%.3f\", $t1 - $t0}")
        b=$(volume_of "$tool" "$WORK/$label.$i.log")
        tarr+=("$t"); rarr+=("$rc"); barr+=("$b")
    done
    # Discard the warmup repetitions (one-time setup) from the statistics.
    for ((j = WARMUP; j < ${#tarr[@]}; j++)); do
        times="$times ${tarr[$j]}"; rcs="$rcs ${rarr[$j]}"; bytes="$bytes ${barr[$j]}"
    done
    # Mean, sample SD, min, max of the run times (sd 0 for a single run).
    read -r mean sd minx maxx <<< "$(awk -v t="$times" 'BEGIN {
        n = split(t, a); s = 0; lo = a[1]; hi = a[1]
        for (i = 1; i <= n; i++) { s += a[i]; if (a[i] < lo) lo = a[i]; if (a[i] > hi) hi = a[i] }
        m = s / n
        v = 0
        if (n > 1) { for (i = 1; i <= n; i++) v += (a[i] - m) ^ 2; v = v / (n - 1) }
        printf "%.3f %.3f %.3f %.3f", m, sqrt(v), lo, hi
    }')"
    local worst=0
    for rc in $rcs; do [ "$rc" -gt "$worst" ] && worst=$rc; done
    local mb
    mb=$(awk -v b="$bytes" 'BEGIN { n = split(b, a); s = 0
        for (i = 1; i <= n; i++) s += a[i]
        if (n > 0) printf "%.0f", s / n; else printf "0" }')
    echo "$mean $sd $minx $maxx $worst $mb"
}

# rsync checksum dry-run: counts files that would be transferred — 0 means
# byte-identical (deliberately deleted sources are not listed without
# --delete).
integrity_diff() { # src rd
    rsync -rltc --dry-run "$1" "$REMOTE:$2" 2>/dev/null | grep -cE '^[-A-Za-z].*\.bin$' || true
}

# A mean±sd cell: "1.77±0.31s".
cell() { printf "%.2f±%.2fs" "$1" "$2"; }

emit_json() { # suite tool scenario mean sd min max runs rc bytes
    [ "$JSON" = 1 ] || return 0
    printf '{"suite":"%s","tool":"%s","scenario":"%s","mean":%s,"sd":%s,"min":%s,"max":%s,"runs":%s,"rc":%s,"bytes":%s}\n' \
        "$1" "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9" "${10}"
}

# ============================================================================
# TREES
# ============================================================================

gen_small_tree() { # dest — SMALL_FILES files, 1-64 KiB, 64 subdirs
    local dst="$1" i=0 sz d
    mkdir -p "$dst"
    while [ "$i" -lt "$SMALL_FILES" ]; do
        d="d$(printf '%02d' $((i / 128)))"
        mkdir -p "$dst/$d"
        sz=$(( (i * 2654435761 % 64512) + 1024 ))   # deterministic sizes
        head -c "$sz" /dev/urandom > "$dst/$d/f$i.bin"
        i=$((i + 1))
    done
}

gen_large_files() { # dest — two 0.5*LARGE_TOTAL_MB files
    local dst="$1"
    mkdir -p "$dst"
    head -c $((LARGE_TOTAL_MB / 2))M /dev/urandom > "$dst/data1.bin"
    head -c $((LARGE_TOTAL_MB / 2))M /dev/urandom > "$dst/data2.bin"
}

mutate_large() { # 1 MiB overwritten mid-file (the delta update)
    dd if=/dev/urandom of="$1/data1.bin" bs=1M seek=$((LARGE_TOTAL_MB / 4)) count=1 conv=notrunc status=none
}

gen_mixed_tree() { # dest — MIX_SMALL/MIX_MEDIUM/MIX_LARGE buckets, ~2 MiB avg
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

mutate_mixed_tree() { # root — 1 K appends + 0.8 K rewrites + 200 new + 100 deleted
    python3 - "$1" "$MIX_SMALL" "$MIX_MEDIUM" "$MIX_LARGE" <<'EOF'
import os, random, sys
root, n_s, n_m, n_l = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
rng = random.Random(7)
def path(i):
    return f"{root}/d{i % 100 + 1}/f{i}.bin"
def mutate_file(i, mode, fn):
    p = path(i)
    if os.path.exists(p):          # repeated mutations must stay idempotent
        with open(p, mode) as f:
            fn(f)
for i in rng.sample(range(1, n_s + 1), min(1000, n_s)):
    mutate_file(i, "ab", lambda f: f.write(b"x" * rng.randint(1, 64)))
for i in rng.sample(range(n_s + 1, n_s + n_m + 1), min(500, n_m)):
    mutate_file(i, "r+b", lambda f: (f.seek(rng.randint(0, 300000)), f.write(os.urandom(16384))))
for i in rng.sample(range(n_s + n_m + 1, n_s + n_m + n_l + 1), min(300, n_l)):
    mutate_file(i, "r+b", lambda f: (f.seek(rng.randint(0, 1500000)), f.write(os.urandom(262144))))
for i in range(n_s + n_m + n_l + 1, n_s + n_m + n_l + 201):
    p = path(i)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "wb") as f:
        f.write(os.urandom(rng.randint(1024, 100000)))
for i in rng.sample(range(1, n_s + n_m + n_l + 1), min(100, n_s)):
    p = path(i)
    if os.path.exists(p):          # repeated mutations must stay idempotent
        os.remove(p)
EOF
}

# ============================================================================
# SUITES
# ============================================================================

cmd_compare() { # [tool ...] — four scenarios, or the mixed phases with MIXED=1
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

    local tool s t mean sd rc bytes
    local -A RESULTS RCS

    if [ "$MIXED" = 1 ]; then
        # The mixed-tree phases: fresh / second / edit + integrity.
        local SRC="$WORK/mixed-src" RW="$WORK/.rw" RD="$REMOTE_BASE/$$/mixed/dst"
        gen_mixed_tree "$SRC"
        mkdir -p "$RW"
        cp -al "$SRC"/. "$RW/"       # hardlink copies: mutate $RW, keep $SRC
        for tool in "${TOOLS[@]}"; do
            ssh "$REMOTE" "mkdir -p ~/$RD/$tool"
        done
        mutate_mixed_here() { mutate_mixed_tree "$RW"; }
        # Rotate the tool order per phase (control-variable interleaving).
        local k off tool
        off=0
        for phase in fresh second edit; do
            for ((k = 0; k < ${#TOOLS[@]}; k++)); do
                tool="${TOOLS[$(((off + k) % ${#TOOLS[@]}))]}"
                local src="$SRC" rd="$RD/$tool" per=1
                case "$phase" in
                    # second/edit re-sync the first fresh destination.
                    second) src="$SRC"; rd="$RD/$tool/r1"; per=0 ;;
                    edit)   src="$RW"; rd="$RD/$tool/r1"; per=0 ;;
                esac
                read -r mean sd minx maxx rc bytes <<< "$(measure "$tool $phase" "$tool" "$src" "$rd" mutate_mixed_here "$per")"
                RESULTS["$tool $phase"]="$mean $sd $rc"
                emit_json compare "$tool" "$phase" "$mean" "$sd" "$minx" "$maxx" "$RUNS" "$rc" "$bytes"
            done
            off=$((off + 1))
        done

        printf "\n%-8s %12s %12s %12s   %s\n" tool fresh second edit integrity
        for tool in "${TOOLS[@]}"; do
            local diffs
            diffs=$(integrity_diff "$RW" "$RD/$tool/r1")
            read -r mean sd rc <<< "${RESULTS[$tool fresh]}"
            printf "%-8s %11s" "$tool" "$(cell "$mean" "$sd")"
            for s in second edit; do
                read -r mean sd rc <<< "${RESULTS[$tool $s]}"
                printf " %11s" "$(cell "$mean" "$sd")"
            done
            printf "   %d differing" "$diffs"
            local bad=""
            for s in fresh second edit; do
                read -r mean sd rc <<< "${RESULTS[$tool $s]}"
                [ "$rc" != 0 ] && bad="$bad $s(rc=$rc)"
            done
            [ -n "$bad" ] && printf "  rc!=0:$bad"
            printf "\n"
        done
        return
    fi

    if [ -z "$SMALL_SRC" ]; then
        SMALL_SRC="$WORK/small-src"
        gen_small_tree "$SMALL_SRC"
    fi
    gen_large_files "$WORK/large"
    echo "large: ${LARGE_TOTAL_MB} MiB in 2 files; small: $SMALL_SRC ($(du -sh "$SMALL_SRC" | cut -f1), $(find "$SMALL_SRC" -type f | wc -l) files)"

    local RD="$REMOTE_BASE/$$/compare"
    ssh "$REMOTE" "mkdir -p ~/$RD"
    # Control-variable setup: identical per-tool sources and isolated remote
    # dests before any measurement.
    declare -A LARGE_SRC
    for tool in "${TOOLS[@]}"; do
        LARGE_SRC[$tool]="$WORK/$tool-large"
        cp -r "$WORK/large" "${LARGE_SRC[$tool]}"
        ssh "$REMOTE" "mkdir -p ~/$RD/$tool/large ~/$RD/$tool/small"
    done
    # The tool order rotates per scenario (each scenario starts with a
    # different tool), so time-correlated drift cannot systematically
    # favor whoever is measured first or last.
    local s k off tool
    local -a SCENARIOS=(large-first large-edit small-first small-idle)
    off=0
    for s in "${SCENARIOS[@]}"; do
        for ((k = 0; k < ${#TOOLS[@]}; k++)); do
            tool="${TOOLS[$(((off + k) % ${#TOOLS[@]}))]}"
            local large="${LARGE_SRC[$tool]}"
            # The edit mutation closes over this tool's copy (the measure
            # hook is called argument-free).
            mutate_large_here() {
                dd if=/dev/urandom of="$large/data1.bin" bs=1M seek=$((LARGE_TOTAL_MB / 4)) count=1 conv=notrunc status=none
            }
            case "$s" in
                large-first) read -r mean sd minx maxx rc bytes <<< "$(measure "$tool $s" "$tool" "$large" "$RD/$tool/large" "" 1)" ;;
                large-edit)  read -r mean sd minx maxx rc bytes <<< "$(measure "$tool $s" "$tool" "$large" "$RD/$tool/large/r1" mutate_large_here)" ;;
                small-first) read -r mean sd minx maxx rc bytes <<< "$(measure "$tool $s" "$tool" "$SMALL_SRC" "$RD/$tool/small" "" 1)" ;;
                small-idle)  read -r mean sd minx maxx rc bytes <<< "$(measure "$tool $s" "$tool" "$SMALL_SRC" "$RD/$tool/small/r1")" ;;
            esac
            RESULTS["$tool $s"]="$mean $sd $rc"
            emit_json compare "$tool" "$s" "$mean" "$sd" "$minx" "$maxx" "$RUNS" "$rc" "$bytes"
        done
        off=$((off + 1))
    done

    printf "\n%-8s %14s %14s %14s %14s   %s\n" tool large-first large-edit small-first small-idle integrity
    local -A FASTEST
    for tool in "${TOOLS[@]}"; do
        printf "%-8s" "$tool"
        for s in large-first large-edit small-first small-idle; do
            read -r mean sd rc <<< "${RESULTS[$tool $s]}"
            printf " %13s" "$(cell "$mean" "$sd")"
            # Track the fastest tool per scenario (by mean).
            if [ -z "${FASTEST[$s]:-}" ]; then
                FASTEST[$s]="$tool $mean"
            else
                read -r ft fm <<< "${FASTEST[$s]}"
                if awk -v a="$mean" -v b="$fm" 'BEGIN{exit !(a < b - 0.0005)}'; then
                    FASTEST[$s]="$tool $mean"
                elif awk -v a="$mean" -v b="$fm" 'BEGIN{exit !(a <= b + 0.0005)}'; then
                    FASTEST[$s]="$ft,$tool $fm"
                fi
            fi
        done
        # Integrity over the final states: the edited large copy and the
        # small tree, both vs their destinations.
        printf "   %d" "$(integrity_diff "$large" "$RD/$tool/large/r1")"
        local small_diff
        small_diff=$(integrity_diff "$SMALL_SRC" "$RD/$tool/small/r1")
        [ "$small_diff" != 0 ] && printf " +%d small" "$small_diff"
        local bad=""
        for s in large-first large-edit small-first small-idle; do
            read -r mean sd rc <<< "${RESULTS[$tool $s]}"
            [ "$rc" != 0 ] && bad="$bad $s(rc=$rc)"
        done
        [ -n "$bad" ] && printf "  rc!=0:$bad"
        printf "\n"
    done
    printf "%-8s" "fastest"
    for s in large-first large-edit small-first small-idle; do
        read -r ft fm <<< "${FASTEST[$s]}"
        printf " %13s" "$ft"
    done
    printf "\n"
    echo
    echo "notes: mean ± sd over $RUNS runs (${WARMUP} warmup repetitions discarded"
    echo "       when WARMUP>0); page cache warmed before each run (WARM=$WARM); the"
    echo "       tool order rotates per scenario (control-variable interleaving, so"
    echo "       time-correlated drift cannot favor a slot); every run bounded by a"
    echo "       ${RUN_TIMEOUT}s timeout with rc recorded; integrity = files a checksum"
    echo "       dry-run would re-transfer (0 = byte-identical)."
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

    # Per-scenario mutation functions (each re-applied before every run, so
    # each repetition is a real delta, not a no-op re-sync).
    mutate_large_src() { dd if=/dev/urandom of="$SRC/data.bin" bs=1M seek=512 count=10 conv=notrunc status=none; }
    mutate_large_src_b() { dd if=/dev/urandom of="$SRC/data.bin" bs=1M seek=256 count=10 conv=notrunc status=none; }
    mutate_large_insert() {
        head -c $((768 * 1048576)) "$SRC/data.bin" > "$WORK/tmp"
        head -c 10M /dev/urandom >> "$WORK/tmp"
        tail -c +$((768 * 1048576 + 1)) "$SRC/data.bin" >> "$WORK/tmp"
        mv "$WORK/tmp" "$SRC/data.bin"
    }
    mutate_small_a() {
        local d
        for d in $(seq 0 $((SMALL_FILES / 128 - 1))); do
            dd if=/dev/urandom of="$SRC/d$(printf '%02d' $d)/f$((d * 128)).bin" bs=1K count=1 conv=notrunc status=none
        done
    }
    mutate_small_b() {
        local d
        for d in $(seq 0 $((SMALL_FILES / 128 - 1))); do
            dd if=/dev/urandom of="$SRC/d$(printf '%02d' $d)/f$((d * 128 + 1)).bin" bs=1K count=1 conv=notrunc status=none
        done
    }

    one_row() { # name mutate_fn lead_tool — a cp2 and an rsync cell side
                 # by side; the lead alternates per row (ABBA ordering)
        local name="$1" mutate="${2:-}" lead="${3:-cp2}" mean sd minx maxx rc
        local cpm rsx
        if [ "$lead" = rsync ]; then
            rsx=$(measure "$name-rsync" rsync "$SRC" "$RD/rsync" "$mutate" 1)
            cpm=$(measure "$name-cp2" cp2 "$SRC" "$RD/cp2" "$mutate" 1)
        else
            cpm=$(measure "$name-cp2" cp2 "$SRC" "$RD/cp2" "$mutate" 1)
            rsx=$(measure "$name-rsync" rsync "$SRC" "$RD/rsync" "$mutate" 1)
        fi
        read -r mean sd minx maxx rc <<< "$cpm"
        printf "%-10s cp2 %13s" "$name" "$(cell "$mean" "$sd")"
        read -r mean sd minx maxx rc <<< "$rsx"
        printf "   rsync %13s\n" "$(cell "$mean" "$sd")"
        read -r mean sd minx maxx rc <<< "$cpm"
        emit_json single cp2 "$name" "$mean" "$sd" "$minx" "$maxx" "$RUNS" "$rc" 0
        read -r mean sd minx maxx rc <<< "$rsx"
        emit_json single rsync "$name" "$mean" "$sd" "$minx" "$maxx" "$RUNS" "$rc" 0
    }

    one_row fresh1 "" cp2
    one_row fresh2 "" rsync
    mutate_mixed_src() { mutate_mixed_tree "$SRC"; }
    if [ "$MODE" = mixed ]; then
        one_row edit mutate_mixed_src cp2
    elif [ "$MODE" = large ]; then
        one_row editA mutate_large_src cp2
        one_row editB mutate_large_src_b rsync
        one_row insert mutate_large_insert cp2
    else
        one_row editA mutate_small_a cp2
        one_row editB mutate_small_b rsync
    fi
    one_row idle "" rsync""
    echo
    echo "notes: fresh = per-run destination (a full transfer each repetition);"
    echo "       edit/insert re-apply the mutation before every run (each"
    echo "       repetition is a real delta against the one destination)."
}

# ---------------------------------------------------------------------------
cmd_daily() { # daily-flow perspective: fresh / idle / edit, MiB/s
    local EDIT_FILES="${EDIT_FILES:-32}"
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

    # The suite targets one destination per tool; the deploy flags ride
    # cp2's argv through the shared dispatcher configured above.
    mutate_edit_files() { # rewrite EDIT_FILES files in place (content + mtime)
        find "$SRC" -type f | sort | head -"$EDIT_FILES" | while read -r f; do
            dd if=/dev/urandom of="$f" bs=1K count=1 conv=notrunc status=none 2>/dev/null || true
        done || true
    }

    one_cell() { # label tool fresh mutate — measure over the suite's dest
        local label="$1" tool="$2" fresh="${3:-0}" mutate="${4:-}" out mean sd rc bytes
        local rd
        [ "$tool" = cp2 ] && rd="$DEST_CP2" || rd="$DEST_RSYNC"
        # fresh runs land on per-run dests (rd/r1..rN); everything else
        # re-syncs the first fresh dest (the delta / no-op base).
        [ "$fresh" = 1 ] || rd="$rd/r1"
        out=$(measure "$label-$tool" "$tool" "$SRC" "$rd" "$mutate" "$fresh")
        read -r mean sd minx maxx rc bytes <<< "$out"
        emit_json daily "$tool" "$label" "$mean" "$sd" "$minx" "$maxx" "$RUNS" "$rc" "$bytes"
        printf "%-9s %-16s %15s  %10.1f MiB  %8.1f MiB/s\n" "$tool" "$label" \
            "$(cell "$mean" "$sd")" \
            "$(awk -v b="$bytes" 'BEGIN{print b/1048576}')" \
            "$(awk -v b="$bytes" -v m="$mean" 'BEGIN{if (m > 0) print b/1048576/m; else print 0}')"
    }

    printf "%-9s %-16s %15s  %12s  %10s\n" tool scenario time bytes speed
    one_cell fresh cp2 1
    one_cell fresh rsync 1
    # idle: the no-op scan; edit: re-mutated before every run — both over
    # the first fresh destination. The leading tool alternates per
    # scenario (ABBA ordering).
    one_cell idle rsync 0
    one_cell idle cp2 0
    one_cell "edit($EDIT_FILES)" cp2 0 mutate_edit_files
    one_cell "edit($EDIT_FILES)" rsync 0 mutate_edit_files
    echo
    echo "notes: bytes = the logical volume each tool materialized (comparable; on a"
    echo "       delta both re-write the touched files in full); fresh = per-run"
    echo "       destination. The first fresh cp2 run includes the auto-deploy when"
    echo "       the remote binary is stale."
}

# ============================================================================
suite="${1:-compare}"
shift || true
case "$suite" in
    compare) cmd_compare "$@" ;;
    single)  cmd_single ;;
    daily)   cmd_daily ;;
    *) echo "unknown suite: $suite (compare | single | remote)" >&2; exit 1 ;;
esac
echo "work dir: $WORK (KEEP_WORK=1 to retain)"