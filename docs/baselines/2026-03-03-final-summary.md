# Performance Optimization Results — 2026-03-03

## Summary of Changes Applied

| # | Change | File(s) |
|---|---|---|
| 1 | Order struct: 104 bytes → 64 bytes (removed stop_price, sentinels, align(64)) | matchx-types |
| 2 | Arena: parallel-array layout (eliminated 128-byte Slot enum overhead) | matchx-arena |
| 3 | Compiler: fat LTO, codegen-units=1, panic=abort, target-cpu=native | Cargo.toml, .cargo/config.toml |
| 4 | Engine: fixed-size inline event buffer (no heap indirection on emit) | matchx-engine |
| 5 | Engine: #[inline(always)] hot path, #[cold] rejection paths | matchx-engine, matchx-types |
| 6 | Engine: flat sorted Vec stop queues (replaced BTreeMap/VecDeque) | matchx-engine |
| 7 | Arena: huge_pages feature (mmap MAP_HUGETLB with 4KB fallback) | matchx-arena |
| 8 | Scripts: PGO workflow, CPU isolation, flamegraph, RT bench Makefile | scripts/, Makefile |

## Benchmark Results (Criterion)

### Before (baseline 2026-03-02)
| Benchmark | p50 (est) |
|---|---|
| insert_limit_order | 21.5 ns |
| crossing_trade | 23.2 ns |
| cancel_order | 5.0 ns |

### After (2026-03-03, fat LTO + all optimizations)
| Benchmark | p50 (est) | Improvement |
|---|---|---|
| insert_limit_order | 17.9 ns | −16.8% |
| crossing_trade | 17.9 ns | −22.9% |
| cancel_order | 5.0 ns | −0.6% (flat) |

## Latency Histogram (1M samples, WSL2 / AMD Ryzen 7 7700)

| Metric | Value |
|---|---|
| p50 | 30 ns |
| p99 | 101 ns |
| p99.9 | 1,502 ns |
| p99.99 | 3,907 ns |
| max | 1,192,959 ns |
| mean | 34.0 ns |
| stddev | 1,388.3 ns |

## p99 vs Sub-µs SLO

- Current p99 on WSL2: **101 ns** (includes OS scheduler jitter)
- Sub-µs SLO target: < 1,000 ns on bare metal with isolcpus
- Status: **Within target** — p99 is well under 1,000 ns even on WSL2; bare metal with isolcpus expected to yield lower p99.9 tail as well

## Next Steps (Phase 2 — Production Readiness)

1. Deploy on bare metal, enable `isolcpus=2,3 nohz_full=2,3 rcu_nocbs=2,3` kernel params
2. Run `make rt-bench` (root required) to get true hardware p99 under SCHED_FIFO
3. If p99 > 1µs: run `make flamegraph` to identify hotspot
4. Enable huge-page arena: `cargo bench --features matchx-arena/huge_pages`
5. Run `bash scripts/pgo-bench.sh` for PGO-optimized measurement
6. Order entry TCP binary gateway (Phase 2 networking layer)
