# matchX - Claude Code Instructions

## Build & Test

```bash
cargo test --workspace          # all tests
cargo clippy --workspace --all-targets -- -D warnings  # lint
cargo bench                     # Criterion benchmarks
cargo bench --bench latency_histogram -- --nocapture   # latency histogram
```

## Crate Overview

| Crate | `no_std` | Description |
|-------|----------|-------------|
| `matchx-types` | yes | Order (64B, align(64)), PriceLevel, MatchEvent, Command |
| `matchx-arena` | yes | Pre-allocated arena, free-list, optional huge pages |
| `matchx-book` | yes | Hybrid dense/sparse order book, Fenwick trees, occupancy bitset |
| `matchx-engine` | yes | Matching engine, PriceTimeFIFO, all order types, STP |
| `matchx-journal` | no | Binary WAL, CRC32, async writer, segment rotation, recovery |
| `matchx-bench` | no | Criterion + HDRHistogram benchmarks |
| `matchx-itests` | no | Integration tests |

## Key Design Decisions

- **Performance over safe abstractions**: Keep `unsafe` with compile-time `const _` static asserts and `debug_assert!` guards. Do NOT replace `unsafe` with safe wrappers that add any overhead (even <1%).
- **`no_std` core crates**: `matchx-types`, `matchx-arena`, `matchx-book`, `matchx-engine` are `no_std` with `extern crate alloc`.
- **64-byte Order**: Exactly one cache line. `#[repr(C, align(64))]` with compile-time size assertion.
- **MaybeUninit event buffer**: Fixed-size `[MaybeUninit<MatchEvent>; 64]` in engine. Bounds proven by static asserts + `debug_assert!`. Do NOT replace with ArrayVec.
- **Deterministic**: Fixed-seed hasher (`XxHash64`), no floating point, logical timestamps. All output is deterministic given same input sequence.

## Code Conventions

- Use `debug_assert!` for invariant checks (zero cost in release)
- Use `const _: () = assert!(...)` for compile-time guarantees
- Annotate every `unsafe` block with `// SAFETY:` comment
- Hot path functions: `#[inline(always)]`; cold/error paths: `#[cold]`
- Sentinel values (`ARENA_NULL`, `STP_NONE`) over `Option` to avoid tag overhead in `Order`
