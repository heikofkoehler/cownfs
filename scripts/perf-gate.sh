#!/bin/bash
# D4: Performance regression gate.
# Runs cownfs-bench --quick, compares metrics against baselines.
# Fails if any metric regresses >20%.
# Baselines are in docs/perf-baseline.txt (update after intentional changes).

set -e
cd "$(dirname "$0")/.."

BASELINE="docs/perf-baseline.txt"
if [ ! -f "$BASELINE" ]; then
    echo "No baseline found at $BASELINE."
    exit 1
fi

# Build release bench.
cargo build --release -p cownfs-bench 2>&1 | tail -1

# Run quick bench, capture output.
OUT=$(./target/release/cownfs-bench --quick 2>&1)

# Extract p99 in nanoseconds from bench output.
# Format: "commit       mean ...  p50 ...  p99 <val><unit>"
extract_p99_ns() {
    local line=$(echo "$OUT" | grep "^$1 " | head -1)
    if [ -z "$line" ]; then
        echo "0"
        return
    fi
    # Normalize µs to us for awk
    echo "$line" | sed 's/µs/us/g' | sed -E 's/.*p99 +([0-9.]+)(ns|us|ms|s).*/\1 \2/' | awk '{
        val=$1; unit=$2;
        if (unit=="ns") printf "%.0f", val;
        else if (unit=="us") printf "%.0f", val*1000;
        else if (unit=="ms") printf "%.0f", val*1000000;
        else if (unit=="s") printf "%.0f", val*1000000000;
        else printf "0";
    }'
}

# Extract IOPS (integer) from bench output.
extract_iops() {
    local line=$(echo "$OUT" | grep "^$1 " | head -1)
    if [ -z "$line" ]; then
        echo "0"
        return
    fi
    # Format: "sync_write       2049 IOPS ..."
    echo "$line" | awk '{for(i=1;i<=NF;i++) if($i=="IOPS") print $(i-1)}' | head -1
}

# Load baseline.
source "$BASELINE"

FAIL=0

check_latency() {
    local name=$1
    local current=$2
    local baseline=$3
    if [ "$baseline" -eq 0 ] || [ "$current" -eq 0 ]; then
        echo "SKIP $name: missing data"
        return
    fi
    # ratio = current * 100 / baseline, using awk for float
    local ratio=$(awk "BEGIN {printf \"%.2f\", $current * 100 / $baseline}")
    local ratio_int=$(echo "$ratio" | cut -d'.' -f1)
    if [ "$ratio_int" -gt 120 ]; then
        echo "FAIL $name: p99 ${current}ns vs baseline ${baseline}ns (${ratio}% > 120%)"
        FAIL=1
    else
        echo "PASS $name: p99 ${current}ns vs baseline ${baseline}ns (${ratio}%)"
    fi
}

check_iops() {
    local name=$1
    local current=$2
    local baseline=$3
    if [ "$baseline" -eq 0 ] || [ "$current" -eq 0 ]; then
        echo "SKIP $name: missing data"
        return
    fi
    local ratio=$(awk "BEGIN {printf \"%.2f\", $current * 100 / $baseline}")
    local ratio_int=$(echo "$ratio" | cut -d'.' -f1)
    if [ "$ratio_int" -lt 80 ]; then
        echo "FAIL $name: ${current} iops vs baseline ${baseline} (${ratio}% < 80%)"
        FAIL=1
    else
        echo "PASS $name: ${current} iops vs baseline ${baseline} (${ratio}%)"
    fi
}

echo "=== Performance Gate ==="
echo ""

CUR=$(extract_p99_ns "commit")
check_latency "commit_p99" "$CUR" "$commit_p99_ns"

CUR=$(extract_p99_ns "sync_write")
check_latency "sync_write_p99" "$CUR" "$sync_write_p99_ns"

CUR=$(extract_iops "sync_write")
check_iops "sync_write_iops" "$CUR" "$sync_write_iops"

CUR=$(extract_p99_ns "alloc_80pct")
check_latency "alloc_80pct_p99" "$CUR" "$alloc_80pct_p99_ns"

echo ""
if [ "$FAIL" -eq 1 ]; then
    echo "PERF GATE FAILED: regression >20% detected"
    exit 1
else
    echo "PERF GATE PASSED"
    exit 0
fi
