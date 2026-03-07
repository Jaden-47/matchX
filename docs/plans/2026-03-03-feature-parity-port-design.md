# Feature Parity Port: OrderBook-rs → matchX

**Date:** 2026-03-03
**Status:** Draft
**Scope:** Port mass cancel, snapshots, analytics, fee system + increase tests + add examples.

## Design Principles

1. **Preserve deterministic single-thread core.** No concurrent data structures, no `Arc`, no locks.
2. **Arena-native.** All operations go through `Arena` + `ArenaIndex`, not heap allocation.
3. **Zero-alloc hot path.** New features that touch the matching path must not allocate.
4. **Separate crates for non-core features.** Analytics, snapshots, fees live outside `matchx-engine`.
5. **Event-driven integration.** New features observe `MatchEvent` stream, don't modify matching internals.

---

## Feature 1: Mass Cancel Operations

### What OrderBook-rs has
- `cancel_all_orders()` — bulk clear, O(L+N)
- `cancel_orders_by_side(side)` — cancel all on one side
- `cancel_orders_by_user(user_id)` — cancel by user (requires per-user index)
- `cancel_orders_by_price_range(side, min, max)` — range cancel

### What matchX needs (adapted)

**New `Command` variants in `matchx-types`:**
```rust
pub enum Command {
    // ... existing
    CancelAll,
    CancelBySide { side: Side },
    CancelByGroup { stp_group: u32 },       // matchX uses stp_group, not user_id
    CancelByPriceRange { side: Side, min_price: u64, max_price: u64 },
}
```

**New `MatchEvent` variant:**
```rust
pub enum MatchEvent {
    // ... existing
    MassCancelComplete { cancelled_count: u32 },
}
```

**Implementation in `matchx-engine`:**
- `cancel_all`: Walk both sides of the book, free every arena slot, clear all levels. Emit individual `OrderCancelled` events + final `MassCancelComplete`. O(N) in orders.
- `cancel_by_side`: Walk one side only. Same emit pattern.
- `cancel_by_group`: Walk all levels on both sides, cancel orders where `order.stp_group == target`. O(N) scan. No per-user index needed (matchX uses `stp_group` as the user-grouping mechanism).
- `cancel_by_price_range`: Use book's dense/sparse structure. Dense range: direct array walk. Sparse range: BTreeMap range scan. Cancel all orders in matching levels.

**Why `stp_group` instead of `user_id`:** matchX doesn't have a `user_id` field on `Order` — it uses `stp_group: u32` as the account-level grouping. This serves the same purpose for mass cancel by account.

### Complexity
- ~200-300 LOC in `matchx-engine/src/lib.rs` (new `process_*` methods)
- ~20 LOC in `matchx-types/src/lib.rs` (new Command/Event variants)

---

## Feature 2: Snapshot & Restore

### What OrderBook-rs has
- `OrderBookSnapshot` — serializable point-in-time state with bids/asks
- `OrderBookSnapshotPackage` — SHA-256 integrity wrapper
- `EnrichedSnapshot` — snapshot + pre-computed metrics (mid price, spread, VWAP, imbalance)
- `MetricFlags` — bitflags for selective metric computation
- JSON serialization + `from_json` restore

### What matchX needs (adapted)

**New crate: `matchx-snapshot`**

```rust
/// Point-in-time capture of book state
pub struct Snapshot {
    pub symbol_id: u32,
    pub sequence: u64,           // from EventMeta
    pub timestamp_ns: u64,
    pub bid_levels: Vec<LevelSnapshot>,
    pub ask_levels: Vec<LevelSnapshot>,
    pub best_bid: Option<u64>,
    pub best_ask: Option<u64>,
}

pub struct LevelSnapshot {
    pub price: u64,
    pub total_quantity: u64,
    pub order_count: u32,
    pub orders: Vec<OrderSnapshot>,  // optional, for full restore
}

pub struct OrderSnapshot {
    pub id: u64,
    pub price: u64,
    pub quantity: u64,
    pub filled: u64,
    pub visible_quantity: u64,
    pub stp_group: u32,
    pub side: Side,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub timestamp: u64,
}
```

**Capture:** Walk the book's dense + sparse structures, reading each level and its order chain via arena traversal. Pure read, no mutation. O(N) in total orders.

**Restore:** Given a `Snapshot`, rebuild `Arena` + `OrderBook` + `MatchingEngine` from scratch. Insert orders in timestamp order to preserve FIFO. Validate with deterministic hash.

**Integrity:** SHA-256 checksum over binary encoding. `SnapshotPackage` wrapper with version + checksum.

**Enriched variant:** Optional metrics struct computed from snapshot data (mid-price, spread, VWAP, imbalance). Computed post-capture, not on hot path.

**Serialization:** `serde` with JSON + optional bincode behind feature flag.

### Complexity
- New crate `matchx-snapshot`: ~400-500 LOC
- Dependencies: `serde`, `serde_json`, `sha2`, optional `bincode`

---

## Feature 3: Market Analytics

### What OrderBook-rs has
- `MarketImpact` — avg price, worst price, slippage, slippage_bps, levels consumed
- `OrderSimulation` — simulated fills without executing
- `DepthStats` — volume, avg size, weighted price, std dev
- `DistributionBin` — histogram of liquidity concentration
- VWAP, spread, micro-price, imbalance calculations
- Functional iterators (cumulative depth, depth-limited, range-based)

### What matchX needs (adapted)

**New crate: `matchx-analytics`**

```rust
/// Market impact simulation (read-only, no mutations)
pub struct MarketImpact {
    pub avg_price: f64,
    pub worst_price: u64,
    pub slippage_ticks: u64,          // in ticks, not basis points
    pub slippage_bps: f64,
    pub levels_consumed: u32,
    pub total_available: u64,
}

/// Simulated order execution
pub struct OrderSimulation {
    pub fills: Vec<(u64, u64)>,       // (price, qty)
    pub avg_price: f64,
    pub total_filled: u64,
    pub remaining: u64,
}

/// Depth statistics per side
pub struct DepthStats {
    pub total_volume: u64,
    pub level_count: u32,
    pub avg_level_size: f64,
    pub weighted_avg_price: f64,
    pub min_level_size: u64,
    pub max_level_size: u64,
    pub std_dev: f64,
}

/// Book-level metrics
pub struct BookMetrics {
    pub best_bid: Option<u64>,
    pub best_ask: Option<u64>,
    pub mid_price: Option<f64>,
    pub spread: Option<u64>,
    pub spread_bps: Option<f64>,
    pub vwap_bid: Option<f64>,        // top N levels
    pub vwap_ask: Option<f64>,
    pub imbalance: f64,               // -1.0 (all asks) to 1.0 (all bids)
}
```

**Implementation approach:**
- All analytics are **read-only** functions that take `&OrderBook` + `&Arena` references.
- `simulate_market_impact(book, arena, side, quantity) -> MarketImpact` — walks the book without mutation, calculates what would happen if a market order of given size hit the book.
- `compute_depth_stats(book, arena, side) -> DepthStats` — aggregate statistics.
- `compute_book_metrics(book, arena, vwap_levels) -> BookMetrics` — combined metrics.
- Leverage matchX's existing Fenwick tree for fast volume queries where possible.

**No functional iterators needed.** matchX's dense+sparse hybrid provides direct access patterns that are more efficient than iterator-based SkipMap traversal. The analytics functions will use the book's native traversal methods.

### Complexity
- New crate `matchx-analytics`: ~300-400 LOC
- Dependencies: `matchx-types`, `matchx-book`, `matchx-arena`

---

## Feature 4: Fee System

### What OrderBook-rs has
- `FeeSchedule` with `maker_fee_bps` / `taker_fee_bps` (i32, negative = rebate)
- `calculate_fee(notional, is_maker) -> i128`
- Integrated into trade emission in `add_order`

### What matchX needs (adapted)

**Add to `matchx-types`:**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeSchedule {
    pub maker_fee_bps: i32,     // basis points, negative = rebate
    pub taker_fee_bps: i32,
}

impl FeeSchedule {
    pub const ZERO: Self = Self { maker_fee_bps: 0, taker_fee_bps: 0 };

    /// Calculate fee for a notional amount. Positive = charge, negative = rebate.
    pub fn calculate_fee(&self, notional: u64, is_maker: bool) -> i64 {
        let bps = if is_maker { self.maker_fee_bps } else { self.taker_fee_bps };
        ((notional as i64) * (bps as i64)) / 10_000
    }
}
```

**Extend `Fill` in `matchx-engine/src/policy.rs`:**

```rust
pub struct Fill {
    pub maker_id: OrderId,
    pub taker_id: OrderId,
    pub price: u64,
    pub quantity: u64,
    pub maker_fee: i64,       // NEW
    pub taker_fee: i64,       // NEW
}
```

**Extend `MatchEvent::Fill`:**
```rust
MatchEvent::Fill {
    meta: EventMeta,
    maker_id: OrderId,
    taker_id: OrderId,
    price: u64,
    quantity: u64,
    maker_fee: i64,           // NEW
    taker_fee: i64,           // NEW
}
```

**Integration:** `FeeSchedule` stored on `InstrumentConfig`. During `match_against_book`, after each fill, compute fees from `price * quantity * bps / 10_000`. Emit in the `Fill` event. No separate module needed — this is ~30 LOC of computation.

### Complexity
- ~40 LOC in `matchx-types` (FeeSchedule struct + methods)
- ~20 LOC in `matchx-engine` (fee calculation in match loop + emit)
- Fields added to `Fill` and `MatchEvent::Fill`

---

## Feature 5: Test Coverage Increase

### Current state
- matchX: 494 LOC of tests across 5 test files
- OrderBook-rs: 13,006 LOC across 20+ test files

### Target areas

**Priority test gaps (from OrderBook-rs test patterns to adopt):**

1. **Modification tests** (~300 LOC target)
   - Cancel non-existent order
   - Cancel already-cancelled order
   - Modify price (cancel+relist)
   - Modify quantity (in-place)
   - Modify to zero quantity

2. **Mass cancel tests** (~200 LOC target)
   - Cancel all on empty book
   - Cancel by side
   - Cancel by group
   - Cancel by price range (dense vs sparse)
   - Cancel range with no matches

3. **Snapshot round-trip tests** (~200 LOC target)
   - Snapshot empty book → restore → verify
   - Snapshot with orders → restore → verify BBO/depth
   - Checksum validation
   - Corrupted snapshot rejection

4. **Edge case matching tests** (~300 LOC target)
   - OrderBook-rs has extensive edge cases in `operations.rs`, `matching.rs`, `book.rs` tests
   - Iceberg refill exhaustion
   - Multiple stop-limit cascade chains
   - STP with all mode × order-type combinations
   - FOK with exactly-sufficient and one-short liquidity

5. **Fee calculation tests** (~100 LOC target)
   - Zero fee
   - Maker rebate
   - Overflow protection
   - Fee in Fill events

6. **Analytics tests** (~150 LOC target)
   - Market impact simulation vs actual execution comparison
   - VWAP accuracy
   - Empty book metrics

**Total target:** ~1,250 LOC new tests, bringing matchX from 494 → ~1,750 LOC.

---

## Feature 6: Examples

### What OrderBook-rs has
20+ examples (7,130 LOC) covering basic usage, HFT simulation, analytics, snapshots, iterators, etc.

### What matchX needs

**Target: 5-7 focused examples in a new `examples/` directory.**

1. **`basic_orderbook.rs`** (~100 LOC) — Create engine, submit limit/market orders, observe fills.
2. **`order_types.rs`** (~150 LOC) — Demonstrate each order type (Limit, Market, PostOnly, IOC, FOK, StopLimit, Iceberg).
3. **`stp_modes.rs`** (~100 LOC) — Show each STP mode in action.
4. **`mass_cancel.rs`** (~80 LOC) — Demonstrate mass cancel operations.
5. **`snapshot_restore.rs`** (~100 LOC) — Take snapshot, restore, verify.
6. **`market_analytics.rs`** (~120 LOC) — Compute and display market metrics, simulate impact.
7. **`journal_replay.rs`** (~120 LOC) — Write-ahead log + deterministic replay.

**Total target:** ~770 LOC across 7 examples.

---

## Crate Dependency Graph (After)

```
matchx-types (no deps, no_std)
    ↑
matchx-arena (matchx-types)
    ↑
matchx-book (matchx-types, matchx-arena, hashbrown, twox-hash)
    ↑
matchx-engine (matchx-types, matchx-arena, matchx-book, smallvec)
    ↑                    ↑                     ↑
matchx-journal       matchx-analytics      matchx-snapshot
    ↑                    ↑                     ↑
matchx-bench         matchx-itests          examples/
```

**New crates:** `matchx-snapshot`, `matchx-analytics`
**Modified crates:** `matchx-types` (FeeSchedule + new Command/Event variants), `matchx-engine` (mass cancel + fee integration)

---

## Implementation Order

1. **Phase 1: Types + Fee System** — Extend `matchx-types` with FeeSchedule, new Command/Event variants. Extend engine with fee calculation. (~60 LOC)
2. **Phase 2: Mass Cancel** — Implement mass cancel operations in engine. (~300 LOC)
3. **Phase 3: Snapshot** — New `matchx-snapshot` crate with capture/restore/integrity. (~500 LOC)
4. **Phase 4: Analytics** — New `matchx-analytics` crate with metrics/simulation. (~400 LOC)
5. **Phase 5: Tests** — Comprehensive test suite for all new + existing features. (~1,250 LOC)
6. **Phase 6: Examples** — 7 runnable examples. (~770 LOC)

**Total new code: ~3,280 LOC** (taking matchX from 5,530 → ~8,810 LOC)

---

## What We Are NOT Porting

These OrderBook-rs features are architectural mismatches for matchX's deterministic core:

- **Concurrent data structures** (DashMap, SkipMap) — matchX is single-threaded by design
- **NATS integration** — infrastructure coupling; can be added later as a separate adapter
- **Implied volatility** — derivatives-specific, not relevant for spot matching
- **Functional iterators** — matchX's dense+sparse book provides more efficient direct access
- **Generic `<T>` extra fields** — adds complexity for flexibility matchX doesn't need
- **UUID order IDs** — matchX uses compact `u64` IDs by design
- **Tokio runtime dependency** — matchX avoids async in the core path
- **TradeListener callback pattern** — matchX uses explicit event buffer return
- **Repricing/special orders** (trailing stop, pegged) — can be added later as Phase 2
