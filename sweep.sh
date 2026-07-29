#!/bin/bash
# Full-picture sweep for disk-write-bench.
#
# Usage: ./sweep.sh [PATH] [DURATION_SECS]
#   ./sweep.sh /mnt/raid0/bench.dat 15
#
# Runs four independent comparisons, each isolating one variable:
#   A. write mechanism      (direct / direct-sync / uring / buffered)
#   B. preallocation x depth  <-- the one that explained the flat plateau
#   C. block size
#   D. first-touch vs preallocated, single-threaded (reconciles the C write() result)
#
# Only `direct` and `direct-sync` bypass the page cache. Buffered modes are run
# with --fsync so their numbers mean "reached the device" rather than "filled RAM".

set -u

BENCH="$(dirname "$0")/target/release/disk-write-bench"
PATH_ARG="${1:-/mnt/raid0/bench.dat}"
DUR="${2:-15}"
CAP="300g"   # bounds the fallocate reservation; raise if the device is faster

[ -x "$BENCH" ] || { echo "build first: cargo build --release" >&2; exit 1; }

run() {
    # run <label> <extra args...>
    local label="$1"; shift
    local out
    out=$("$BENCH" --path "$PATH_ARG" --duration "$DUR" --max-bytes "$CAP" "$@" 2>&1)
    local thr peak
    thr=$(echo "$out"  | awk -F': ' '/throughput/{print $2}')
    peak=$(echo "$out" | awk -F': ' '/peak in-flgt/{print $2}')
    if [ -z "$thr" ]; then
        printf '  %-34s ERROR\n' "$label"
        echo "$out" | sed 's/^/      /' | tail -4
        return
    fi
    printf '  %-34s %-34s peak-in-flight=%s\n' "$label" "$thr" "${peak:-n/a}"
    rm -f "$PATH_ARG"
}

echo "=============================================================="
echo " disk-write-bench sweep"
echo " path=$PATH_ARG  duration=${DUR}s  cap=$CAP"
echo " $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
echo "=============================================================="
echo
echo "--- A. write mechanism (preallocated, qd=32, bs=8m) ----------"
echo "    buffered modes use --fsync so the number reaches the device"
run "direct (O_DIRECT+io_uring)"  --mode direct      --queue-depth 32
run "direct-sync (O_DIRECT+write)" --mode direct-sync
run "uring (buffered+io_uring)"   --mode uring       --queue-depth 32 --fsync
run "buffered (write(2))"         --mode buffered    --fsync
echo
echo "--- B. preallocation x queue depth (direct, bs=8m) -----------"
echo "    the headline comparison: does depth do anything?"
for q in 1 2 4 8 16 32 64; do
    run "prealloc    qd=$q" --mode direct --queue-depth "$q"
done
echo
for q in 1 2 4 8 16 32 64; do
    run "no-prealloc qd=$q" --mode direct --queue-depth "$q" --no-prealloc
done
echo
echo "--- C. block size (direct, preallocated, qd=32) --------------"
for bs in 1m 2m 4m 8m 16m 64m; do
    run "bs=$bs" --mode direct --queue-depth 32 --block-size "$bs"
done
echo
echo "--- D. single-threaded, first-touch vs preallocated ----------"
echo "    mirrors a plain C O_DIRECT write() loop"
run "direct-sync, no-prealloc" --mode direct-sync --no-prealloc
run "direct-sync, prealloc"    --mode direct-sync
echo
echo "=============================================================="
echo " done"
echo "=============================================================="
