# matchX

High-performance matching engine targeting sub-microsecond p99 latency. Written in Rust (`no_std` core).

## Architecture

```
matchx-types     Core types: Order (64B cache-line), PriceLevel, events, commands
matchx-arena     Pre-allocated arena with free-list reuse, optional huge pages
matchx-book      Hybrid dense/sparse order book with Fenwick trees + occupancy bitset
matchx-engine    PriceTimeFIFO matching, all order types, STP, stop-limit cascading
matchx-journal   Binary WAL with CRC32, async background writer, segment rotation
matchx-bench     Criterion benchmarks + HDRHistogram latency reporting
matchx-itests    Integration tests (replay determinism, async WAL)
```

## Order Types

- **Limit** (GTC / IOC / FOK)
- **Market**
- **Post-Only** (rejected if would cross spread)
- **Stop-Limit** (cascading trigger support)
- **Iceberg** (automatic visible-slice replenishment)

## Self-Trade Prevention

Four modes: `CancelNewest`, `CancelOldest`, `CancelBoth`, `DecrementAndCancel`.

## Performance

- Order struct: exactly 64 bytes, `align(64)` (one cache line)
- Arena: zero-allocation hot path, optional `mmap(MAP_HUGETLB)` backing
- Book: O(1) BBO via occupancy bitset (`leading_zeros`/`trailing_zeros`), O(log N) depth queries via Fenwick trees
- Engine: fixed-size inline event buffer, `#[inline(always)]` hot paths, flat sorted Vec for stop queues
- Build: fat LTO, `panic=abort`, PGO-ready

## Quick Start

```bash
# Run all tests
cargo test --workspace

# Run benchmarks
cargo bench

# Latency histogram (p50/p99/p99.9)
cargo bench --bench latency_histogram -- --nocapture

# Clippy
cargo clippy --workspace --all-targets -- -D warnings
```

## Project Structure

```
crates/
  matchx-types/       Shared types (Order, PriceLevel, MatchEvent, Command)
  matchx-arena/       Pre-allocated Order arena
  matchx-book/        Hybrid order book (dense ticks + sparse BTreeMap)
  matchx-engine/      Matching engine + MatchPolicy trait
  matchx-journal/     Event sourcing WAL (sync + async)
  matchx-bench/       Benchmarks
  matchx-itests/      Integration tests
docs/
  plans/              Design docs and implementation plans
  baselines/          Benchmark baseline records
scripts/              PGO, CPU isolation, RT bench scripts
```

## Safety

All `unsafe` blocks are documented in [SAFETY.md](SAFETY.md). Debug builds include generation counters for use-after-free detection. CI runs Miri for unsafe validation.

## License

MIT
