# OrderBook-rs vs matchX Parity Matrix

**Date:** 2026-03-03  
**Status:** Research Complete  
**Scope:** Core matching differences, data structures, determinism/concurrency model, and feature parity.

## Repositories Compared

- `matchX`: `/home/jaden/workspace/matchX`
- `OrderBook-rs`: `/home/jaden/workspace/OrderBook-rs`

## Methodology

- Direct source inspection only (no external web claims).
- Confidence labels:
  - `High`: directly verified in source files listed below.
  - `Medium`: inferred from nearby modules/tests/docs.

## System Snapshot (Quantitative)

| Metric | matchX | OrderBook-rs |
|---|---:|---:|
| Rust files (src/tests/benches scope used) | ~22 | ~88 |
| Total LOC in compared Rust files | ~5,530 | ~28,790 |
| Dominant module size | `matchx-engine/src/lib.rs` (~1,634 LOC) | `orderbook/book.rs` (~2,946 LOC) |
| Packaging shape | Multi-crate workspace | Single main crate with broad modules |

**Interpretation:** OrderBook-rs is broader in scope. matchX is narrower but with deep core-engine specialization.

## Core Matching Parity Matrix

| Capability | matchX | OrderBook-rs | Gap | Confidence | Key References |
|---|---|---|---|---|---|
| Engine architecture | Single-thread deterministic event loop returning `&[MatchEvent]` | Shared concurrent book with lock-free/concurrent structures and `MatchResult` flows | Major | High | `crates/matchx-engine/src/lib.rs`, `src/orderbook/matching.rs`, `src/orderbook/book.rs` |
| Core order book structure | Hybrid dense+sparse levels + Fenwick + occupancy bitset + recentering | Concurrent `SkipMap` + `DashMap` + `Arc<PriceLevel>` | Major | High | `crates/matchx-book/src/lib.rs`, `src/orderbook/book.rs` |
| Hot-path order storage | Fixed-size arena (`Order`=64 bytes) + intrusive prev/next links | Delegated to `pricelevel` crate objects, wrapped in concurrent maps | Major | High | `crates/matchx-types/src/lib.rs`, `crates/matchx-arena/src/lib.rs`, `src/orderbook/private.rs`, `src/orderbook/book.rs` |
| Matching rule baseline | Price-time FIFO policy trait (`MatchPolicy`) | Price-level matching delegated via `pricelevel::PriceLevel::match_order` | Partial | High | `crates/matchx-engine/src/policy.rs`, `src/orderbook/matching.rs` |
| Limit/Market/IOC/FOK | Supported | Supported | None | High | `crates/matchx-engine/src/lib.rs`, `src/orderbook/operations.rs`, `src/orderbook/book.rs`, `src/orderbook/matching.rs` |
| Post-only | Supported | Supported | None | High | `crates/matchx-engine/src/lib.rs`, `src/orderbook/operations.rs`, `src/orderbook/modifications.rs` |
| Iceberg | Supported | Supported | None | High | `crates/matchx-types/src/lib.rs`, `crates/matchx-engine/src/lib.rs`, `src/orderbook/operations.rs`, `src/orderbook/modifications.rs` |
| Stop-limit | Supported in engine queue with trigger drain on last trade price | Not a direct equivalent in current core API (special orders are trailing/pegged under feature) | Partial | Medium | `crates/matchx-engine/src/lib.rs`, `src/orderbook/repricing.rs`, `src/orderbook/mod.rs` |
| Extended order types | Limited to core exchange set | Broader set: trailing stop, pegged, market-to-limit, reserve | Major | High | `crates/matchx-types/src/lib.rs`, `src/orderbook/private.rs`, `src/orderbook/repricing.rs` |
| STP modes | `CancelNewest`, `CancelOldest`, `CancelBoth`, `DecrementAndCancel` | `None`, `CancelTaker`, `CancelMaker`, `CancelBoth` | Major | High | `crates/matchx-types/src/lib.rs`, `crates/matchx-engine/src/lib.rs`, `src/orderbook/stp.rs`, `src/orderbook/matching.rs` |
| FOK liquidity precheck | Dense+sparse indexed quantity queries (`ask_available_at_or_below`, `bid_available_at_or_above`) | `peek_match` precheck before execution | Partial | High | `crates/matchx-book/src/lib.rs`, `crates/matchx-engine/src/lib.rs`, `src/orderbook/matching.rs`, `src/orderbook/modifications.rs` |
| Determinism guarantees in core path | Explicit deterministic hasher + event sequence/timestamp + replay tests | Concurrency-first path; determinism support appears in sequencer subsystem | Major | High | `crates/matchx-book/src/lib.rs`, `crates/matchx-engine/tests/properties.rs`, `crates/matchx-itests/tests/replay_determinism.rs`, `src/orderbook/sequencer/mod.rs`, `src/orderbook/sequencer/types.rs` |
| Journaling & replay | Dedicated crate (`matchx-journal`) plus integration tests | Sequencer/journal subsystem (feature-gated file journal) | Partial | High | `crates/matchx-journal/src/lib.rs`, `crates/matchx-journal/src/async_journal.rs`, `src/orderbook/sequencer/file_journal.rs` |
| Snapshot/restore | Present in journal/recovery flows | Rich snapshot package/json restore support | Partial | High | `crates/matchx-journal/src/recovery.rs`, `tests/unit/snapshot_restore_tests.rs`, `src/orderbook/snapshot.rs` |
| Mass cancel operations | Basic cancel by order ID in engine path | Rich mass cancel: all, side, user, price range | Major | High | `crates/matchx-engine/src/lib.rs`, `src/orderbook/mass_cancel.rs` |
| Market analytics (VWAP/spread/micro-price/impact) | Minimal in core engine; benchmark/latency metrics crate exists | Extensive analytics and market-impact utilities | Major | High | `crates/matchx-bench/src/metrics.rs`, `src/orderbook/book.rs`, `src/orderbook/market_impact.rs`, `src/orderbook/statistics.rs` |
| Multi-book orchestration | Not present as a unified manager abstraction | `BookManager` (std + tokio variants) | Major | High | `src/orderbook/manager.rs`, `src/lib.rs` |
| NATS integration | Not present | Feature-gated NATS publishers for trade/book-change events | Major | High | `src/orderbook/nats.rs`, `src/orderbook/nats_book_change.rs`, `src/orderbook/mod.rs` |

## Core Data Type Differences

### matchX (performance-first, deterministic core)

- `Order`: fixed-size cacheline-oriented struct with intrusive links and explicit remaining/matchable semantics.
- `ArenaIndex(u32)`: compact index into preallocated arena.
- `PriceLevel`: minimal linked-queue metadata and aggregate quantity.
- Engine emits explicit `MatchEvent` with monotonic `EventMeta`.

References:
- `crates/matchx-types/src/lib.rs`
- `crates/matchx-arena/src/lib.rs`
- `crates/matchx-book/src/lib.rs`
- `crates/matchx-engine/src/lib.rs`

### OrderBook-rs (feature breadth + concurrent access)

- Uses `pricelevel` crate primitives (`OrderType<T>`, `PriceLevel`, `Id`, `TimeInForce`) with generic extra fields.
- Book internals center around concurrent maps (`SkipMap`, `DashMap`) and shared `Arc<PriceLevel>`.
- Matching flow returns `MatchResult` and optionally emits listeners.

References:
- `src/orderbook/book.rs`
- `src/orderbook/private.rs`
- `src/orderbook/operations.rs`
- `src/orderbook/matching.rs`

### Consequence Summary

- matchX favors predictable memory layout and deterministic replay behavior.
- OrderBook-rs favors extensibility and concurrent multi-feature utility surface.

## Behavioral Differences (Matching Semantics)

1. STP semantics are not one-to-one.
- matchX has `CancelNewest/Oldest/Both/DecrementAndCancel`.
- OrderBook-rs has `CancelTaker/Maker/Both` and user-ID enforcement when STP enabled.

2. Stop-order philosophy differs.
- matchX has direct `StopLimit` queue and trigger drain logic in engine.
- OrderBook-rs core exposes broader special-order framework (repricing/feature-gated), but no direct equivalent to matchX stop queue semantics in the inspected API.

3. Event contract differs.
- matchX surfaces event stream (`MatchEvent`) as first-class deterministic output.
- OrderBook-rs surfaces `MatchResult` with richer ecosystem hooks (listeners, snapshots, publishers).

References:
- `crates/matchx-engine/src/lib.rs`
- `crates/matchx-types/src/lib.rs`
- `src/orderbook/stp.rs`
- `src/orderbook/matching.rs`
- `src/orderbook/repricing.rs`

## Prioritized Recommendations for matchX

### P0 (Keep/strengthen now)

1. Preserve deterministic single-thread core and replay invariants as primary contract.
2. Keep arena + hybrid book architecture; continue proving correctness via property/integration replay tests.
3. Maintain explicit event model (`MatchEvent`) as stable audit/replay primitive.

References:
- `crates/matchx-engine/tests/properties.rs`
- `crates/matchx-itests/tests/replay_determinism.rs`
- `crates/matchx-itests/tests/async_wal_replay_determinism.rs`

### P1 (Adopt selectively for exchange operations)

1. Add richer mass-cancel operations (by user/side/range) in a bounded API.
2. Add explicit per-user order index APIs if needed for operational tooling.
3. Consider configurable tick/lot/min/max validation interfaces at book/engine boundary.

Potential inspiration:
- `src/orderbook/mass_cancel.rs`
- `src/orderbook/modifications.rs`
- `src/orderbook/book.rs`

### P2 (Add later, not hot-path first)

1. Optional analytics module (VWAP/spread/imbalance/impact) isolated from matching path.
2. Optional publisher integrations (e.g., NATS) behind feature flags.
3. Optional multi-book manager abstraction if product architecture requires it.

Potential inspiration:
- `src/orderbook/statistics.rs`
- `src/orderbook/market_impact.rs`
- `src/orderbook/nats.rs`
- `src/orderbook/manager.rs`


## Conclusion

matchX is currently **focused and engine-centric**; OrderBook-rs is **broader and platform-like**.  
For a low-latency deterministic matching core, matchX is already architected in the right direction.  
The practical roadmap is not “copy everything,” but “adopt selected operational breadth features without compromising deterministic core behavior.”
