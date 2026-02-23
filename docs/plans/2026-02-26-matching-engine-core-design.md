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

## Order Book Structure (Hybrid Tick-Array + Sparse Levels + Arena)

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

    // Dense tick window near BBO for O(1) hot-path access
    bids_dense: Vec<PriceLevel>,
    asks_dense: Vec<PriceLevel>,
    dense_base_price: u64,
    dense_max_ticks: u32,

    // Sparse overflow for far-from-BBO prices
    bids_sparse: BTreeMap<u64, PriceLevel>,
    asks_sparse: BTreeMap<u64, PriceLevel>,

    // Best price tracking (maintained incrementally)
    best_bid: Option<u64>,
    best_ask: Option<u64>,

    // Occupancy bitset for O(1) next-best lookup after level empties
    // One bit per dense tick; find next occupied via leading_zeros/trailing_zeros
    bids_occupied: Vec<u64>,  // ceil(dense_max_ticks / 64) words
    asks_occupied: Vec<u64>,

    // Side depth index for sublinear FOK checks
    bid_depth_index: FenwickTree<u64>,
    ask_depth_index: FenwickTree<u64>,

    // Order lookup by ID (fixed-seed hasher for determinism)
    order_index: HashMap<OrderId, ArenaIndex>,

    // Stop orders indexed by stop price for range-trigger activation
    stop_bids: BTreeMap<u64, VecDeque<ArenaIndex>>,
    stop_asks: BTreeMap<u64, VecDeque<ArenaIndex>>,
    next_stop_bid_trigger: Option<u64>,
    next_stop_ask_trigger: Option<u64>,

    sequence: u64,
}
```

**Dense window sizing:** Full-range tick arrays are too expensive for wide or drifting price ranges. For BTC/USDT at $100k with $0.01 tick = 10M ticks (~240MB per side at 24 bytes/level), use dense arrays only near BBO.

**Hybrid level policy:** Keep a configurable dense window (e.g., 64K–512K ticks) centered near BBO, and store far levels in sparse `BTreeMap`. Recenter when BBO moves past a threshold (for example 70% of window width), migrating levels in bounded batches.

**Best price tracking:** Maintained incrementally using a per-side **occupancy bitset** (one bit per dense tick). When the best price level empties, the next-best is found via `trailing_zeros` (asks) or `leading_zeros` (bids) on the bitset words — O(dense_max_ticks / 64) worst case, typically O(1). Sparse levels are consulted only when no occupied dense tick is found. This avoids the latency-busting linear scan that a naive dense-array walk would require.

**Dense window recentering:** When BBO drifts past 70% of the dense window width, trigger a recenter operation that migrates levels between dense and sparse storage. Migration is performed in bounded batches (configurable, e.g., 4K ticks per batch) to cap worst-case latency. Recentering updates the occupancy bitsets and Fenwick trees atomically. Orders that migrate from dense to sparse (or vice versa) retain their intrusive list linkage — only the level container changes.

**Indexed stop/FOK checks:** Stop activation uses `BTreeMap::range` from last trigger cursor (`O(log N + K)` for `K` triggered stops). FOK pre-check in phase 1 uses Fenwick/prefix sums for dense levels (`O(log N)`) plus linear sparse-range summation; phase 2 adds a sparse range-volume index for full sublinear checks.

## Matching Engine & Pluggable Rules

### MatchPolicy Trait

```rust
trait FillSink {
    fn push_fill(&mut self, fill: Fill);
}

trait MatchPolicy {
    fn match_order(
        &self,
        incoming: &Order,
        level: &mut PriceLevel,
        orders: &mut Arena<Order>,
        fill_sink: &mut dyn FillSink,
    );

    fn is_price_acceptable(
        &self,
        incoming_side: Side,
        incoming_price: u64,
        resting_price: u64,
    ) -> bool;
}
```

Default implementation: `PriceTimeFIFO`.

Engine passes a preallocated fill sink (`SmallVec`-backed or arena-backed) to keep matching hot-path allocation-free under normal load. The engine owns a reusable event output buffer (`Vec<MatchEvent>`) that is cleared and reused on each `process()` call, returning `&[MatchEvent]` to avoid per-call heap allocation.

### Matching Flow

1. **Pre-trade validation:** instrument limits, order size, STP group check
2. **Stop trigger check:** after each fill, update `last_trade_price`; activate stop orders whose trigger condition transitioned to true via indexed range query and trigger cursors (`O(log N + K)`)
3. **Match against resting book:**
   - Market: walk opposing side from best price until filled or exhausted
   - Limit: walk while price acceptable
   - IOC: limit match, cancel remainder
   - FOK: phase 1 queries dense depth index plus sparse range sum for available-volume pre-check; reject if insufficient, else fill all
   - Post-Only: reject if would cross spread
   - Iceberg: fill visible, replenish from hidden (back of queue)
4. **Modify:** cancel-and-replace semantics — remove existing order, then route the replacement through the full new-order path (including matching against the opposing book). This ensures a modify that crosses the spread produces correct fills rather than resting silently.
5. **Post-match:** remainder to book (GTC) or cancelled. Update BBO. Emit `BookUpdate` events for every price level whose quantity changed.

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

All events carry `EventMeta { sequence: u64, timestamp_ns: u64 }` — monotonic output sequence and deterministic logical clock (not wall-clock). The engine must populate `EventMeta` on every emitted event; `sequence` increments per event, `timestamp_ns` increments per event and is persisted in snapshots.

## Event Sourcing & Deterministic Replay

### Command Log

```rust
enum Command {
    NewOrder { ... },
    CancelOrder { id },
    ModifyOrder { id, new_price, new_qty },
}
```

`TriggerAuction` and explicit `Snapshot` commands are out of scope for this core milestone and belong to control-plane/orchestration phases.

Each command gets `input_sequence: u64` at entry. Sequence (not wall-clock) determines execution order.

### Journal Format

- Segment header (fixed 64B): `magic`, `version`, `shard_id`, `instrument_id`, `segment_index`, `start_input_sequence`, `created_at_ns`, `header_crc32c`
- Record format: `[record_len: u32][record_type: u16][flags: u16][input_sequence: u64][command bytes][record_crc32c: u32]`
- Command payload uses canonical binary schema (SBE-style) with explicit endianness and schema version
- Append/write policy: sequential append, optional group-commit window, configurable `fsync` policy
- Segment trailer: `last_committed_sequence`, `record_count`, `segment_hash`
- Segments rotated at 256MB with strictly monotonic `segment_index`

### Replay

- Startup recovery scans segments in order, validates checksums, and truncates from first invalid/torn record to last valid boundary
- Feed only valid committed records sequentially into matching engine
- Output must be byte-identical across runs
- Determinism via: deterministic dense-index + ordered sparse-map traversal, fixed-seed hasher, no wall-clock in matching, no floating point

### Determinism Contract

- `input_sequence` is the sole execution-order source of truth.
- `EventMeta.timestamp_ns` is a logical engine clock (increment-by-one per emitted event), persisted in snapshots and never sourced from wall-clock.
- Price-level traversal order is stable: bids high-to-low, asks low-to-high, FIFO within level using intrusive list order.
- `HashMap` usage requires a fixed hasher seed; randomized seeds are forbidden.
- Matching and replay code must not depend on wall-clock time, random numbers, unordered container iteration, or floating-point behavior.

### Snapshots

- Periodic full order book state snapshots with canonical deterministic serialization order
- Snapshot metadata includes `snapshot_sequence`, `input_sequence`, `segment_index`, and `segment_offset`
- Snapshot commit protocol: write temp file, `fsync` file + directory, atomic rename to committed snapshot
- Recovery: load latest committed snapshot + replay journal from stored segment/offset
- Includes state hash and rolling event-hash anchor for integrity verification

### Hash Chain

Use a cryptographic rolling hash over canonical event bytes for tamper detection and continuity verification:

- `H_0 = BLAKE3("matchx:event-chain:v1" || genesis_anchor)`
- `H_n = BLAKE3(H_{n-1} || canonical_event_bytes_n)`
- Persist hash anchors in each segment trailer and snapshot metadata.
- Recovery/replay verifies uninterrupted anchor continuity and fails hard on mismatch.
- CRC remains for corruption/torn-write detection; hash chain covers semantic tampering.

## Project Structure

```
matchx/
├── Cargo.toml              (workspace)
├── crates/
│   ├── matchx-types/       # Shared types
│   ├── matchx-arena/       # Arena allocator + free list
│   ├── matchx-book/        # OrderBook (hybrid dense+sparse levels)
│   ├── matchx-engine/      # MatchingEngine + MatchPolicy
│   ├── matchx-journal/     # Event journal, replay, snapshots
│   └── matchx-bench/       # Benchmarks
├── docs/plans/
└── tests/                  # Integration + replay tests
```

### Rust Choices

- `#![no_std]` compatible for types, arena, book crates
- `unsafe` only in arena (tested with Miri)
- Fixed-seed hasher for `HashMap<OrderId, ArenaIndex>` — using `twox-hash::XxHash64` with explicit seed 0; verified with cross-platform hash stability test
- `#[repr(C)]` with explicit cache-line padding on hot-path structs
- `rust-toolchain.toml` at workspace root pinning MSRV for reproducible builds

## Testing Strategy

- **proptest:** Bid/ask never cross, fill quantity conservation, sequence monotonicity, cancel-of-nonexistent rejected, replay determinism
- **cargo-fuzz:** Random byte sequences as commands (dedicated setup task)
- **Determinism test:** Same input twice, byte-identical output
- **Benchmarks:** criterion (micro), iai-callgrind (CI regression), hdrhistogram (latency distribution)

### CI Requirements

- `cargo fmt --check` on all crates
- `cargo clippy -- -D warnings` on all crates
- `cargo +nightly miri test -p matchx-arena` for unsafe validation
- `cargo-fuzz` smoke run (bounded iterations)
- Benchmark baseline artifact storage for regression gating

### Performance Acceptance

- Benchmark environment: pinned isolated core, fixed CPU governor/performance profile, warm cache run after deterministic warmup.
- Core latency gate for crossing match path: `P50 < 1us`, `P99 < 3us` under the documented benchmark workload.
- CI regression gate: fail on >10% P99 regression versus baseline artifact for the same hardware profile.
- Report must include throughput, `P50/P95/P99`, and allocation counts on hot path.
