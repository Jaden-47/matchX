# Matching Engine Core Design

**Date:** 2026-02-26
**Status:** Approved
**Scope:** Single-shard matching engine core for crypto exchange

## Context

Crypto exchange matching engine targeting sub-microsecond (<1us) per-operation latency. Full order type support, pluggable matching rules, deterministic event sourcing from day one.

## Requirements

- **Product:** Centralized crypto exchange matching engine
- **Order types:** Limit, Market, Cancel, Modify, IOC, FOK, GTC, Post-Only, Iceberg, Stop-Limit, Self-trade prevention
- **Latency target:** <1us per match operation
- **Matching rules:** Pluggable via `MatchPolicy` trait; Price-Time FIFO as default
- **Event sourcing:** Command journal, deterministic replay, periodic snapshots

## Data Model

### Price & Quantity

Fixed-point integers. Prices in ticks (`u64`), quantities in lots (`u64`). Each instrument defines `tick_size` and `lot_size`. No floating point anywhere in matching logic.

### Order

```rust
struct Order {
    id: OrderId,              // u64, monotonic
    side: Side,               // Bid | Ask
    price: u64,               // ticks
    quantity: u64,            // lots
    filled: u64,             // lots
    order_type: OrderType,
    time_in_force: TimeInForce,
    timestamp: u64,           // nanoseconds, monotonic
    visible_quantity: u64,    // for Iceberg
    stop_price: Option<u64>,  // for Stop-Limit
    stp_group: Option<u32>,   // self-trade prevention group
    // Arena linkage (intrusive doubly-linked list)
    prev: Option<ArenaIndex>,
    next: Option<ArenaIndex>,
}
```

`ArenaIndex` is a `u32` index into a pre-allocated `Vec<Order>` with free-list reuse. Zero heap allocation on insert/cancel.

## Order Book Structure (Tick-Array + Arena)

### Price Level

```rust
struct PriceLevel {
    total_quantity: u64,
    order_count: u32,
    head: Option<ArenaIndex>,
    tail: Option<ArenaIndex>,
}
```

### Order Book

```rust
struct OrderBook {
    instrument_id: u32,
    tick_size: u64,
    lot_size: u64,

    // Tick-indexed arrays for O(1) price level access
    bids: Vec<PriceLevel>,
    asks: Vec<PriceLevel>,
    base_price: u64,
    max_ticks: u32,

    // Best price tracking (maintained incrementally)
    best_bid: Option<u64>,
    best_ask: Option<u64>,

    // Order lookup by ID (fixed-seed hasher for determinism)
    order_index: HashMap<OrderId, ArenaIndex>,

    // Stop orders (sorted by stop price)
    stop_bids: BTreeMap<u64, VecDeque<ArenaIndex>>,
    stop_asks: BTreeMap<u64, VecDeque<ArenaIndex>>,

    sequence: u64,
}
```

**Tick array sizing:** For BTC/USDT at $100k with $0.01 tick = 10M ticks, ~240MB per side at 24 bytes/level. Acceptable for high-value instruments. Configurable per instrument.

**Best price tracking:** Maintained on insert/cancel. When a level empties, scan to next occupied level (amortized O(1) since levels are dense near BBO).

## Matching Engine & Pluggable Rules

### MatchPolicy Trait

```rust
trait MatchPolicy {
    fn match_order(
        &self,
        incoming: &Order,
        level: &PriceLevel,
        orders: &Arena<Order>,
    ) -> Vec<Fill>;

    fn is_price_acceptable(
        &self,
        incoming_side: Side,
        incoming_price: u64,
        resting_price: u64,
    ) -> bool;
}
```

Default implementation: `PriceTimeFIFO`.

### Matching Flow

1. **Pre-trade validation:** instrument limits, order size, STP group check
2. **Stop trigger check:** if execution changes BBO, scan stop orders for triggers
3. **Match against resting book:**
   - Market: walk opposing side from best price until filled or exhausted
   - Limit: walk while price acceptable
   - IOC: limit match, cancel remainder
   - FOK: pre-check total available, reject if insufficient, else fill all
   - Post-Only: reject if would cross spread
   - Iceberg: fill visible, replenish from hidden (back of queue)
4. **Post-match:** remainder to book (GTC) or cancelled. Update BBO. Emit events.

### Self-Trade Prevention

Orders with same `stp_group` don't trade. Configurable: cancel-newest, cancel-oldest, cancel-both, decrement-and-cancel.

### Output Events

```rust
enum MatchEvent {
    OrderAccepted { id, side, price, qty, ... },
    OrderRejected { id, reason },
    Fill { maker_id, taker_id, price, qty, ... },
    OrderCancelled { id, remaining_qty },
    OrderModified { id, new_price, new_qty },
    BookUpdate { side, price, qty_delta },
    StopTriggered { stop_id, new_order_id },
}
```

All events carry monotonic `sequence` and nanosecond timestamp.

## Event Sourcing & Deterministic Replay

### Command Log

```rust
enum Command {
    NewOrder { ... },
    CancelOrder { id },
    ModifyOrder { id, new_price, new_qty },
    TriggerAuction { instrument_id },
    Snapshot { instrument_id },
}
```

Each command gets `input_sequence: u64` at entry. Sequence (not wall-clock) determines execution order.

### Journal Format

- Append-only binary: `[length: u32][input_sequence: u64][command bytes][crc32: u32]`
- Flat binary layout (SBE-style), fixed-size structs where possible
- `mmap` + sequential write; configurable `fsync` policy
- Segments rotated at 256MB

### Replay

- Feed journal commands sequentially into matching engine
- Output must be byte-identical across runs
- Determinism via: tick-array indexing (no iteration order dependency), fixed-seed hasher, no wall-clock in matching, no floating point

### Snapshots

- Periodic full order book state snapshots
- Recovery: load snapshot + replay journal from snapshot sequence
- Includes state hash for integrity verification

### Hash Chain

Rolling hash over output events for tamper detection and consistency verification.

## Project Structure

```
matchx/
├── Cargo.toml              (workspace)
├── crates/
│   ├── matchx-types/       # Shared types
│   ├── matchx-arena/       # Arena allocator + free list
│   ├── matchx-book/        # OrderBook (tick-array)
│   ├── matchx-engine/      # MatchingEngine + MatchPolicy
│   ├── matchx-journal/     # Event journal, replay, snapshots
│   └── matchx-bench/       # Benchmarks
├── docs/plans/
└── tests/                  # Integration + replay tests
```

### Rust Choices

- `#![no_std]` compatible for types, arena, book crates
- `unsafe` only in arena (tested with Miri)
- Fixed-seed hasher for `HashMap<OrderId, ArenaIndex>`
- `#[repr(C)]` with explicit cache-line padding on hot-path structs

## Testing Strategy

- **proptest:** Bid/ask never cross, fill quantity conservation, sequence monotonicity, cancel-of-nonexistent rejected, replay determinism
- **cargo-fuzz:** Random byte sequences as commands
- **Determinism test:** Same input twice, byte-identical output
- **Benchmarks:** criterion (micro), iai-callgrind (CI regression), hdrhistogram (latency distribution)
