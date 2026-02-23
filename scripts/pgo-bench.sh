#!/usr/bin/env bash
# scripts/pgo-bench.sh — Profile-Guided Optimization for matchx-bench
# Usage: bash scripts/pgo-bench.sh
# Requires: rustup component add llvm-tools-preview
#           cargo install cargo-pgo (optional, for convenience)
#
# Manual steps this script automates:
#   1. Build with LLVM instrumentation to collect branch profile data
#   2. Run the instrumented bench binary to generate .profraw files
#   3. Merge profiles into .profdata with llvm-profdata
#   4. Rebuild with the merged profile for PGO-optimized output

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$SCRIPT_DIR/.."
PROFILE_DIR="$ROOT/pgo-profiles"
MERGED="$PROFILE_DIR/merged.profdata"

echo "=== PGO Bench Workflow ==="
echo ""

# Step 1: Check prerequisites
if ! rustup component list --installed 2>/dev/null | grep -q "llvm-tools"; then
    echo "ERROR: llvm-tools-preview not installed."
    echo "Run: rustup component add llvm-tools-preview"
    exit 1
fi

LLVM_PROFDATA=$(find "$(rustup toolchain list -v | grep '(default)' | awk '{print $2}')" \
    -name "llvm-profdata" 2>/dev/null | head -1)
if [[ -z "$LLVM_PROFDATA" ]]; then
    echo "ERROR: llvm-profdata not found. Install with: rustup component add llvm-tools-preview"
    exit 1
fi

echo "Using llvm-profdata: $LLVM_PROFDATA"
echo ""

# Step 2: Instrument build + collect profiles
echo "=== Step 1: Building instrumented bench binary ==="
rm -rf "$PROFILE_DIR" && mkdir -p "$PROFILE_DIR"

pushd "$ROOT" > /dev/null
INSTR_BIN=$(RUSTFLAGS="-C instrument-coverage -C target-cpu=native" \
    cargo bench --bench matching --no-run --message-format=json 2>/dev/null \
    | python3 -c "import sys,json; [print(json.loads(l)['executable']) for l in sys.stdin if 'executable' in l and json.loads(l).get('executable')]" \
    | tail -1)

if [[ -z "$INSTR_BIN" ]]; then
    echo "ERROR: Could not find instrumented bench binary."
    exit 1
fi

echo "Instrumented binary: $INSTR_BIN"
echo ""
echo "=== Step 2: Running instrumented bench to collect profile data ==="
LLVM_PROFILE_FILE="$PROFILE_DIR/bench-%p-%m.profraw" "$INSTR_BIN" --bench 2>/dev/null || true

PROFRAW_COUNT=$(find "$PROFILE_DIR" -name "*.profraw" | wc -l)
echo "Collected $PROFRAW_COUNT .profraw file(s)"
if [[ "$PROFRAW_COUNT" -eq 0 ]]; then
    echo "ERROR: No .profraw files generated. Check that the bench binary ran successfully."
    exit 1
fi

echo ""
echo "=== Step 3: Merging profiles ==="
"$LLVM_PROFDATA" merge -sparse "$PROFILE_DIR"/*.profraw -o "$MERGED"
echo "Merged profile: $MERGED"

echo ""
echo "=== Step 4: PGO-optimized benchmark run ==="
mkdir -p "$ROOT/docs/baselines"
OUTFILE="$ROOT/docs/baselines/$(date +%Y-%m-%d)-after-pgo.txt"
RUSTFLAGS="-C profile-use=$MERGED -C target-cpu=native" \
    cargo bench 2>&1 | tee "$OUTFILE"

popd > /dev/null
echo ""
echo "=== Done. Results saved to $OUTFILE ==="
