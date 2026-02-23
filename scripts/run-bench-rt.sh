#!/usr/bin/env bash
# Run the latency histogram benchmark under SCHED_FIFO on an isolated CPU.
#
# Requirements:
#   - Root or sudo access (for chrt)
#   - CPU isolation active (run scripts/setup-cpu-isolation.sh first)
#   - Build the bench binary first: cargo bench --bench latency_histogram --no-run
#
# Usage: sudo bash scripts/run-bench-rt.sh [cpu_number]
# Default CPU: 2 (adjust to match your isolcpus= setting)

set -euo pipefail

ISOLATED_CPU="${1:-2}"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "=== RT Latency Benchmark ==="
echo "Isolated CPU: $ISOLATED_CPU"
echo ""

# Find the compiled bench binary
BENCH_BIN=$(find "$ROOT_DIR/target" -name "latency_histogram-*" -type f \
    ! -name "*.d" ! -name "*.rmeta" 2>/dev/null \
    | xargs -I{} sh -c 'test -x "{}" && echo "{}"' 2>/dev/null \
    | head -1)

if [[ -z "$BENCH_BIN" ]]; then
    echo "Bench binary not found. Building..."
    cd "$ROOT_DIR"
    cargo bench --bench latency_histogram --no-run 2>&1 | tail -5
    BENCH_BIN=$(find "$ROOT_DIR/target" -name "latency_histogram-*" -type f \
        ! -name "*.d" ! -name "*.rmeta" 2>/dev/null \
        | xargs -I{} sh -c 'test -x "{}" && echo "{}"' 2>/dev/null \
        | head -1)
fi

if [[ -z "$BENCH_BIN" ]]; then
    echo "ERROR: Could not find latency_histogram bench binary."
    exit 1
fi

echo "Binary: $BENCH_BIN"
echo ""

OUTFILE="$ROOT_DIR/docs/baselines/$(date +%Y-%m-%d)-rt-latency.txt"
mkdir -p "$ROOT_DIR/docs/baselines"

echo "Running with SCHED_FIFO priority 99 on CPU $ISOLATED_CPU..."
taskset -c "$ISOLATED_CPU" chrt -f 99 "$BENCH_BIN" --nocapture \
    2>&1 | tee "$OUTFILE"

echo ""
echo "Results saved to: $OUTFILE"
