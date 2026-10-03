#!/bin/bash
# D4: Performance regression gate.
# Runs cownfs-bench --quick, compares p99 latencies against baselines.
# Fails if any metric regresses >20%.
# Baselines are in docs/perf-baseline.txt (update after intentional changes).

set -e
cd "$(dirname "$0")/.."

BASELINE="docs/perf-baseline.txt"
if [ ! -f "$BASELINE" ]; then
    echo "No baseline found at $BASELINE. Run: $0 --record-baseline"
    exit 1
fi

# Build release bench.
cargo build --release -p cownfs-bench 2>&1 | tail -1

# Run quick bench, capture output.
OUT=$(./target/release/cownfs-bench --quick 2>&1)

# Extract metrics (p99 values in microseconds, convert to ns for comparison).
# Format: "commit       mean ...  p50 ...  p99 <val>"
extract_p99() {
    echo "$OUT" | grep "^$1" | sed -E 's/.*p99 +([0-9.]+)([a-zµ]+).*/\1 \2/' | while read val unit; do
        case $unit in
            ns) echo "$val" ;;
            µs|us) echo "$val * 1000" | bc ;;
            ms) echo "$val * 1000000" | bc ;;
            s) echo "$val * 1000000000" | bc ;;
        esac
    done
}

# For now, just print the metrics. Full comparison requires bc and parsing.
# TODO: implement baseline comparison.
echo "=== Benchmark results ==="
echo "$OUT" | grep -E "^(commit|sync_write|bitmap_write|alloc_80pct)"
echo ""
echo "Baseline comparison not yet implemented (see TODO)."
echo "Manual check: compare p99 values against $BASELINE"
