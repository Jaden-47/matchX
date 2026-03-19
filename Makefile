.PHONY: all baseline bench latency flamegraph rt-bench test

# Default target: run tests
all: test

# Run all tests
test:
	cargo test --workspace

# Record baseline benchmark numbers (Criterion)
baseline:
	cargo bench 2>&1 | tee docs/baselines/$$(date +%Y-%m-%d)-baseline.txt

# Run Criterion benchmarks
bench:
	cargo bench

# Lint with Clippy (deny warnings)
lint:
	cargo clippy --workspace --all-targets -- -D warnings

# Run Miri for unsafe validation (requires nightly)
miri:
	cargo +nightly miri test --workspace

# Run latency histogram benchmark (no RT scheduling)
latency:
	cargo bench --bench latency_histogram -- --nocapture

# Generate flamegraph SVG (requires: cargo install flamegraph, linux-perf)
# Usage: make flamegraph
flamegraph:
	CARGO_PROFILE_BENCH_DEBUG=true \
	RUSTFLAGS="-C force-frame-pointers=yes -C target-cpu=native" \
	cargo flamegraph --bench matching -- --bench
	@echo "Flamegraph written to flamegraph.svg"

# Run latency benchmark under SCHED_FIFO on isolated CPU (requires root + isolcpus)
# Usage: make rt-bench  (must be run as root or with sudo)
rt-bench:
	@bash scripts/setup-cpu-isolation.sh
	sudo bash scripts/run-bench-rt.sh
