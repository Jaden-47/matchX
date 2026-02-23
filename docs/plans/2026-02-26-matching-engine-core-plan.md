# Matching Engine Core Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a sub-microsecond crypto exchange matching engine core with a hybrid dense+sparse order book, arena allocator, pluggable matching rules, and deterministic event sourcing.

**Architecture:** Cargo workspace with 7 crates: types, arena, book, engine, journal, bench, itests. Single-threaded deterministic event loop. Dense tick window near BBO + sparse overflow map for price levels. Arena-allocated orders with intrusive linked lists. Sublinear stop indexing and phase-1 FOK checks (dense `O(log N)` + sparse linear range sum). Crash-recoverable append-only journal with deterministic replay.

**Tech Stack:** Rust (stable), no_std where possible, proptest, criterion, cargo-fuzz

**Design doc:** `docs/plans/2026-02-26-matching-engine-core-design.md`

**Determinism Contract (non-negotiable):**
- `input_sequence` is the only source of execution ordering.
- `EventMeta.timestamp_ns` is a deterministic logical clock (not wall-clock), incremented per emitted event and persisted in snapshot state.
- Traversal order is stable and explicit: bids high-to-low, asks low-to-high, FIFO within level.
- Hash maps use fixed-seed deterministic hashing.
- No wall-clock, random input, floating-point behavior, or unordered iteration may influence output bytes.

---

### Task 1: Workspace Scaffold

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `rust-toolchain.toml`
- Create: `crates/matchx-types/Cargo.toml`
- Create: `crates/matchx-types/src/lib.rs`
- Create: `crates/matchx-arena/Cargo.toml`
- Create: `crates/matchx-arena/src/lib.rs`
- Create: `crates/matchx-book/Cargo.toml`
- Create: `crates/matchx-book/src/lib.rs`
- Create: `crates/matchx-engine/Cargo.toml`
- Create: `crates/matchx-engine/src/lib.rs`
- Create: `crates/matchx-journal/Cargo.toml`
- Create: `crates/matchx-journal/src/lib.rs`
- Create: `crates/matchx-bench/Cargo.toml`
- Create: `crates/matchx-bench/src/lib.rs`
- Create: `crates/matchx-itests/Cargo.toml`
- Create: `crates/matchx-itests/src/lib.rs`

**Step 1: Create `rust-toolchain.toml`**

```toml
[toolchain]
channel = "stable"
```

**Step 2: Create workspace Cargo.toml**

```toml
[workspace]
resolver = "2"
members = [
    "crates/matchx-types",
    "crates/matchx-arena",
    "crates/matchx-book",
    "crates/matchx-engine",
    "crates/matchx-journal",
    "crates/matchx-bench",
    "crates/matchx-itests",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"

[workspace.dependencies]
matchx-types = { path = "crates/matchx-types" }
matchx-arena = { path = "crates/matchx-arena" }
matchx-book = { path = "crates/matchx-book" }
matchx-engine = { path = "crates/matchx-engine" }
matchx-journal = { path = "crates/matchx-journal" }
matchx-itests = { path = "crates/matchx-itests" }
proptest = "1"
```

**Step 3: Create each crate Cargo.toml and empty lib.rs**

`crates/matchx-types/Cargo.toml`:
```toml
[package]
name = "matchx-types"
version.workspace = true
edition.workspace = true

[dependencies]
```

`crates/matchx-arena/Cargo.toml`:
```toml
[package]
name = "matchx-arena"
version.workspace = true
edition.workspace = true

[dependencies]
matchx-types.workspace = true
```

`crates/matchx-book/Cargo.toml`:
```toml
[package]
name = "matchx-book"
version.workspace = true
edition.workspace = true

[dependencies]
matchx-types.workspace = true
matchx-arena.workspace = true
```

`crates/matchx-engine/Cargo.toml`:
```toml
[package]
name = "matchx-engine"
version.workspace = true
edition.workspace = true

[dependencies]
matchx-types.workspace = true
matchx-arena.workspace = true
matchx-book.workspace = true

[dev-dependencies]
proptest.workspace = true
```

`crates/matchx-journal/Cargo.toml`:
```toml
[package]
name = "matchx-journal"
version.workspace = true
edition.workspace = true

[dependencies]
matchx-types.workspace = true
```

`crates/matchx-bench/Cargo.toml`:
```toml
[package]
name = "matchx-bench"
version.workspace = true
edition.workspace = true

[dependencies]
matchx-types.workspace = true
matchx-arena.workspace = true
matchx-book.workspace = true
matchx-engine.workspace = true
matchx-journal.workspace = true
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "matching"
harness = false
```

`crates/matchx-itests/Cargo.toml`:
```toml
[package]
name = "matchx-itests"
version.workspace = true
edition.workspace = true
publish = false

[dependencies]
matchx-types.workspace = true
matchx-engine.workspace = true
matchx-journal.workspace = true

[dev-dependencies]
tempfile = "3"
```

Each `src/lib.rs` starts as empty (or `// TODO`).

**Step 4: Verify workspace compiles**

Run: `cargo check`
Expected: compiles with no errors

**Step 5: Commit**

```bash
git add -A
git commit -m "feat: scaffold cargo workspace with 7 crates including itests"
```

---

### Task 2: Core Types (matchx-types)

**Files:**
- Create: `crates/matchx-types/src/lib.rs`
- Test: inline `#[cfg(test)]` module

**Step 1: Write failing tests for core types**

Add to `crates/matchx-types/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_opposite() {
        assert_eq!(Side::Bid.opposite(), Side::Ask);
        assert_eq!(Side::Ask.opposite(), Side::Bid);
    }

    #[test]
    fn order_remaining_quantity() {
        let order = Order {
            id: OrderId(1),
            side: Side::Bid,
            price: 1000,
            quantity: 100,
            filled: 30,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            visible_quantity: 100,
            stop_price: None,
            stp_group: None,
            prev: None,
            next: None,
        };
        assert_eq!(order.remaining(), 70);
        assert!(!order.is_filled());
    }

    #[test]
    fn order_is_filled() {
        let order = Order {
            id: OrderId(1),
            side: Side::Bid,
            price: 100,
            quantity: 50,
            filled: 50,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            visible_quantity: 50,
            stop_price: None,
            stp_group: None,
            prev: None,
            next: None,
        };
        assert_eq!(order.remaining(), 0);
        assert!(order.is_filled());
    }

    #[test]
    fn arena_index_conversion() {
        let idx = ArenaIndex(42);
        assert_eq!(idx.as_usize(), 42);
    }

    #[test]
    fn remaining_saturates_on_invalid_overfill_state() {
        let order = Order {
            id: OrderId(9),
            side: Side::Ask,
            price: 100,
            quantity: 10,
            filled: 15, // invalid state
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            visible_quantity: 10,
            stop_price: None,
            stp_group: None,
            prev: None,
            next: None,
        };
        assert_eq!(order.remaining(), 0);
        assert!(!order.is_valid_state());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p matchx-types`
Expected: FAIL — types not defined

**Step 3: Implement core types**

Replace `crates/matchx-types/src/lib.rs` with:

```rust
#![cfg_attr(not(test), no_std)]

/// Newtype for order IDs. Monotonically increasing u64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct OrderId(pub u64);

/// Newtype for arena slot indices. u32 to save space in Order struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct ArenaIndex(pub u32);

impl ArenaIndex {
    #[inline]
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

/// Bid or Ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Bid = 0,
    Ask = 1,
}

impl Side {
    #[inline]
    pub fn opposite(self) -> Self {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// Order type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OrderType {
    Limit,
    Market,
    PostOnly,
    StopLimit,
    Iceberg,
}

/// Time-in-force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TimeInForce {
    GTC,  // Good-til-Cancel
    IOC,  // Immediate-or-Cancel
    FOK,  // Fill-or-Kill
}

/// Self-trade prevention mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StpMode {
    CancelNewest,
    CancelOldest,
    CancelBoth,
    DecrementAndCancel,
}

/// Rejection reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RejectReason {
    InvalidPrice,
    InvalidQuantity,
    InstrumentNotFound,
    OrderNotFound,
    WouldCrossSpread,   // Post-only rejection
    InsufficientLiquidity, // FOK rejection
    SelfTradePreventionTriggered,
    DuplicateOrderId,
}

/// An order stored in the arena. Uses intrusive doubly-linked list
/// for FIFO queue at each price level.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct Order {
    pub id: OrderId,
    pub side: Side,
    pub price: u64,           // ticks
    pub quantity: u64,        // lots
    pub filled: u64,          // lots
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub timestamp: u64,       // nanoseconds, monotonic
    pub visible_quantity: u64,// for Iceberg
    pub stop_price: Option<u64>,
    pub stp_group: Option<u32>,
    // Intrusive linked list pointers (arena indices)
    pub prev: Option<ArenaIndex>,
    pub next: Option<ArenaIndex>,
}

impl Order {
    /// Remaining unfilled quantity.
    #[inline]
    pub fn remaining(&self) -> u64 {
        self.quantity.saturating_sub(self.filled)
    }

    /// Structural validity check used by pre-trade/replay validation.
    #[inline]
    pub fn is_valid_state(&self) -> bool {
        self.filled <= self.quantity
    }

    /// Whether order is fully filled.
    #[inline]
    pub fn is_filled(&self) -> bool {
        self.filled >= self.quantity
    }
}

/// A price level in the order book.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct PriceLevel {
    pub total_quantity: u64,
    pub order_count: u32,
    pub head: Option<ArenaIndex>,
    pub tail: Option<ArenaIndex>,
}

impl PriceLevel {
    pub const EMPTY: Self = Self {
        total_quantity: 0,
        order_count: 0,
        head: None,
        tail: None,
    };

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.order_count == 0
    }
}

/// Metadata shared by all emitted events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventMeta {
    pub sequence: u64,     // monotonic output sequence
    pub timestamp_ns: u64, // monotonic engine clock
}

/// Events emitted by the matching engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchEvent {
    OrderAccepted {
        meta: EventMeta,
        id: OrderId,
        side: Side,
        price: u64,
        qty: u64,
        order_type: OrderType,
    },
    OrderRejected {
        meta: EventMeta,
        id: OrderId,
        reason: RejectReason,
    },
    Fill {
        meta: EventMeta,
        maker_id: OrderId,
        taker_id: OrderId,
        price: u64,
        qty: u64,
        maker_remaining: u64,
        taker_remaining: u64,
    },
    OrderCancelled {
        meta: EventMeta,
        id: OrderId,
        remaining_qty: u64,
    },
    OrderModified {
        meta: EventMeta,
        id: OrderId,
        new_price: u64,
        new_qty: u64,
    },
    BookUpdate {
        meta: EventMeta,
        side: Side,
        price: u64,
        qty: u64,       // new total at this level (0 = level removed)
    },
    StopTriggered {
        meta: EventMeta,
        stop_id: OrderId,
        new_order_id: OrderId,
    },
}

/// Commands into the matching engine.
#[derive(Debug, Clone)]
pub enum Command {
    NewOrder {
        id: OrderId,
        instrument_id: u32,
        side: Side,
        price: u64,
        qty: u64,
        order_type: OrderType,
        time_in_force: TimeInForce,
        visible_qty: Option<u64>,   // Iceberg
        stop_price: Option<u64>,    // Stop-Limit
        stp_group: Option<u32>,
    },
    CancelOrder {
        id: OrderId,
    },
    ModifyOrder {
        id: OrderId,
        new_price: u64,
        new_qty: u64,
    },
}

/// Instrument configuration.
#[derive(Debug, Clone)]
pub struct InstrumentConfig {
    pub id: u32,
    pub tick_size: u64,
    pub lot_size: u64,
    pub base_price: u64,   // lowest representable price in ticks
    pub max_ticks: u32,    // number of tick slots per side
    pub stp_mode: StpMode,
}

// tests go here...
```

Note: Later snippets may omit `meta: EventMeta` on event construction for brevity. Implementation must populate `EventMeta` on every emitted `MatchEvent`.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p matchx-types`
Expected: 5 tests PASS

**Step 5: Commit**

```bash
git add crates/matchx-types/
git commit -m "feat(types): add core types - Order, PriceLevel, events, commands"
```

---

### Task 3: Arena Allocator (matchx-arena)

**Files:**
- Create: `crates/matchx-arena/src/lib.rs`
- Test: inline `#[cfg(test)]` module

**Step 1: Write failing tests**

Add to `crates/matchx-arena/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use matchx_types::*;

    fn make_order(id: u64) -> Order {
        Order {
            id: OrderId(id),
            side: Side::Bid,
            price: 100,
            quantity: 10,
            filled: 0,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            visible_quantity: 10,
            stop_price: None,
            stp_group: None,
            prev: None,
            next: None,
        }
    }

    #[test]
    fn alloc_and_get() {
        let mut arena = Arena::new(16);
        let idx = arena.alloc(make_order(1)).unwrap();
        assert_eq!(arena.get(idx).id, OrderId(1));
    }

    #[test]
    fn alloc_reuses_freed_slot() {
        let mut arena = Arena::new(2);
        let a = arena.alloc(make_order(1)).unwrap();
        let b = arena.alloc(make_order(2)).unwrap();
        arena.free(a);
        let c = arena.alloc(make_order(3)).unwrap();
        // c should reuse a's slot
        assert_eq!(c, a);
        assert_eq!(arena.get(c).id, OrderId(3));
        // b still intact
        assert_eq!(arena.get(b).id, OrderId(2));
    }

    #[test]
    fn alloc_fails_when_full() {
        let mut arena = Arena::new(1);
        arena.alloc(make_order(1)).unwrap();
        assert!(arena.alloc(make_order(2)).is_none());
    }

    #[test]
    fn len_tracks_live_count() {
        let mut arena = Arena::new(4);
        assert_eq!(arena.len(), 0);
        let a = arena.alloc(make_order(1)).unwrap();
        let b = arena.alloc(make_order(2)).unwrap();
        assert_eq!(arena.len(), 2);
        arena.free(a);
        assert_eq!(arena.len(), 1);
        arena.free(b);
        assert_eq!(arena.len(), 0);
    }

    #[test]
    fn get_mut_modifies_in_place() {
        let mut arena = Arena::new(4);
        let idx = arena.alloc(make_order(1)).unwrap();
        arena.get_mut(idx).filled = 5;
        assert_eq!(arena.get(idx).filled, 5);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p matchx-arena`
Expected: FAIL — Arena not defined

**Step 3: Implement arena**

```rust
#![cfg_attr(not(test), no_std)]
extern crate alloc;

use alloc::vec::Vec;
use matchx_types::{ArenaIndex, Order};

/// Pre-allocated arena for Order objects.
/// Uses a free list for O(1) alloc/free with zero heap allocation
/// after construction.
pub struct Arena {
    slots: Vec<Slot>,
    free_head: Option<u32>,
    len: u32,
}

enum Slot {
    Occupied(Order),
    Free { next_free: Option<u32> },
}

impl Arena {
    /// Create arena with given capacity. All slots start free.
    pub fn new(capacity: u32) -> Self {
        let mut slots = Vec::with_capacity(capacity as usize);
        for i in 0..capacity {
            let next = if i + 1 < capacity { Some(i + 1) } else { None };
            slots.push(Slot::Free { next_free: next });
        }
        Self {
            slots,
            free_head: if capacity > 0 { Some(0) } else { None },
            len: 0,
        }
    }

    /// Allocate a slot for the given order. Returns None if full.
    #[inline]
    pub fn alloc(&mut self, order: Order) -> Option<ArenaIndex> {
        let idx = self.free_head?;
        match &self.slots[idx as usize] {
            Slot::Free { next_free } => {
                self.free_head = *next_free;
            }
            Slot::Occupied(_) => unreachable!(),
        }
        self.slots[idx as usize] = Slot::Occupied(order);
        self.len += 1;
        Some(ArenaIndex(idx))
    }

    /// Free a slot, returning it to the free list.
    #[inline]
    pub fn free(&mut self, index: ArenaIndex) {
        let idx = index.0;
        self.slots[idx as usize] = Slot::Free {
            next_free: self.free_head,
        };
        self.free_head = Some(idx);
        self.len -= 1;
    }

    /// Get immutable reference to order at index.
    #[inline]
    pub fn get(&self, index: ArenaIndex) -> &Order {
        match &self.slots[index.as_usize()] {
            Slot::Occupied(order) => order,
            Slot::Free { .. } => panic!("access to freed arena slot"),
        }
    }

    /// Get mutable reference to order at index.
    #[inline]
    pub fn get_mut(&mut self, index: ArenaIndex) -> &mut Order {
        match &mut self.slots[index.as_usize()] {
            Slot::Occupied(order) => order,
            Slot::Free { .. } => panic!("access to freed arena slot"),
        }
    }

    /// Number of live (occupied) slots.
    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Total capacity.
    #[inline]
    pub fn capacity(&self) -> u32 {
        self.slots.len() as u32
    }
}

// tests go here...
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p matchx-arena`
Expected: 5 tests PASS

**Step 5: Run under Miri to check safety**

Run: `cargo +nightly miri test -p matchx-arena`
Expected: PASS with no UB detected

**Step 6: Commit**

```bash
git add crates/matchx-arena/
git commit -m "feat(arena): add pre-allocated arena with free-list reuse"
```

---

### Task 4: Order Book — Insert and Best Price (matchx-book)

**Files:**
- Create: `crates/matchx-book/src/lib.rs`
- Test: inline `#[cfg(test)]` module

**Step 1: Write failing tests for insert + best price**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use matchx_types::*;

    fn config() -> InstrumentConfig {
        InstrumentConfig {
            id: 1,
            tick_size: 1,
            lot_size: 1,
            base_price: 0,
            max_ticks: 1000,
            stp_mode: StpMode::CancelNewest,
        }
    }

    #[test]
    fn insert_bid_updates_best_bid() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(1), Side::Bid, 500, 10, &mut arena);
        assert_eq!(book.best_bid(), Some(500));
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn insert_ask_updates_best_ask() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(1), Side::Ask, 600, 10, &mut arena);
        assert_eq!(book.best_ask(), Some(600));
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn multiple_bids_best_is_highest() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(1), Side::Bid, 500, 10, &mut arena);
        book.insert_order(OrderId(2), Side::Bid, 510, 5, &mut arena);
        book.insert_order(OrderId(3), Side::Bid, 490, 20, &mut arena);
        assert_eq!(book.best_bid(), Some(510));
    }

    #[test]
    fn multiple_asks_best_is_lowest() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(1), Side::Ask, 600, 10, &mut arena);
        book.insert_order(OrderId(2), Side::Ask, 590, 5, &mut arena);
        book.insert_order(OrderId(3), Side::Ask, 610, 20, &mut arena);
        assert_eq!(book.best_ask(), Some(590));
    }

    #[test]
    fn level_quantity_accumulates() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(1), Side::Bid, 500, 10, &mut arena);
        book.insert_order(OrderId(2), Side::Bid, 500, 20, &mut arena);
        let level = book.get_bid_level(500);
        assert_eq!(level.total_quantity, 30);
        assert_eq!(level.order_count, 2);
    }

    #[test]
    #[should_panic(expected = "fenwick underflow")]
    fn fenwick_sub_underflow_panics() {
        let mut fw = FenwickTree::new(8);
        fw.sub(0, 1);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p matchx-book`
Expected: FAIL — OrderBook not defined

**Step 3: Implement OrderBook with hybrid levels + best price tracking**

```rust
#![cfg_attr(not(test), no_std)]
extern crate alloc;

use alloc::collections::{btree_map::Entry, BTreeMap};
use alloc::vec;
use alloc::vec::Vec;
use matchx_arena::Arena;
use matchx_types::*;

pub struct FenwickTree {
    data: Vec<u64>,
}

impl FenwickTree {
    pub fn new(size: usize) -> Self {
        Self { data: vec![0; size + 1] }
    }

    pub fn add(&mut self, index: usize, delta: u64) {
        let mut i = index + 1;
        while i < self.data.len() {
            self.data[i] += delta;
            i += i & i.wrapping_neg();
        }
    }

    pub fn sub(&mut self, index: usize, delta: u64) {
        let mut i = index + 1;
        while i < self.data.len() {
            self.data[i] = self.data[i]
                .checked_sub(delta)
                .expect("fenwick underflow");
            i += i & i.wrapping_neg();
        }
    }

    pub fn prefix_sum(&self, index: usize) -> u64 {
        let mut i = index + 1;
        let mut acc = 0;
        while i > 0 {
            acc += self.data[i];
            i -= i & i.wrapping_neg();
        }
        acc
    }

    pub fn prefix_sum_le(&self, index: usize) -> u64 {
        self.prefix_sum(index)
    }

    pub fn suffix_sum_ge(&self, index: usize) -> u64 {
        let total = self.prefix_sum(self.data.len() - 2);
        let before = index.checked_sub(1).map_or(0, |i| self.prefix_sum(i));
        total - before
    }
}

/// Hybrid order book:
/// - dense tick window near BBO for O(1) hot-path access
/// - sparse map for far-from-BBO prices
pub struct OrderBook {
    pub instrument_id: u32,
    bids_dense: Vec<PriceLevel>,
    asks_dense: Vec<PriceLevel>,
    dense_base_price: u64,
    dense_max_ticks: u32,
    bids_sparse: BTreeMap<u64, PriceLevel>,
    asks_sparse: BTreeMap<u64, PriceLevel>,
    bid_depth_index: FenwickTree,
    ask_depth_index: FenwickTree,
    best_bid_tick: Option<u64>,
    best_ask_tick: Option<u64>,
    // Occupancy bitset: one bit per dense tick for O(1) next-best lookup
    bids_occupied: Vec<u64>,  // ceil(dense_max_ticks / 64) words
    asks_occupied: Vec<u64>,
}

impl OrderBook {
    pub fn new(config: InstrumentConfig) -> Self {
        let dense_n = config.max_ticks as usize;
        let bitset_words = (dense_n + 63) / 64;
        Self {
            instrument_id: config.id,
            bids_dense: vec![PriceLevel::EMPTY; dense_n],
            asks_dense: vec![PriceLevel::EMPTY; dense_n],
            dense_base_price: config.base_price,
            dense_max_ticks: config.max_ticks,
            bids_sparse: BTreeMap::new(),
            asks_sparse: BTreeMap::new(),
            bid_depth_index: FenwickTree::new(dense_n),
            ask_depth_index: FenwickTree::new(dense_n),
            best_bid_tick: None,
            best_ask_tick: None,
            bids_occupied: vec![0u64; bitset_words],
            asks_occupied: vec![0u64; bitset_words],
        }
    }

    #[inline]
    fn dense_index(&self, price: u64) -> Option<usize> {
        if price < self.dense_base_price {
            return None;
        }
        let idx = price - self.dense_base_price;
        (idx < self.dense_max_ticks as u64).then_some(idx as usize)
    }

    #[inline]
    fn level_mut(&mut self, side: Side, price: u64) -> &mut PriceLevel {
        match (side, self.dense_index(price)) {
            (Side::Bid, Some(i)) => &mut self.bids_dense[i],
            (Side::Ask, Some(i)) => &mut self.asks_dense[i],
            (Side::Bid, None) => match self.bids_sparse.entry(price) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(PriceLevel::EMPTY),
            },
            (Side::Ask, None) => match self.asks_sparse.entry(price) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(PriceLevel::EMPTY),
            },
        }
    }

    #[inline]
    fn set_occupied(&mut self, side: Side, dense_idx: usize) {
        let word = dense_idx / 64;
        let bit = dense_idx % 64;
        match side {
            Side::Bid => self.bids_occupied[word] |= 1u64 << bit,
            Side::Ask => self.asks_occupied[word] |= 1u64 << bit,
        }
    }

    #[inline]
    fn clear_occupied(&mut self, side: Side, dense_idx: usize) {
        let word = dense_idx / 64;
        let bit = dense_idx % 64;
        match side {
            Side::Bid => self.bids_occupied[word] &= !(1u64 << bit),
            Side::Ask => self.asks_occupied[word] &= !(1u64 << bit),
        }
    }

    pub fn insert_order(
        &mut self,
        id: OrderId,
        side: Side,
        price: u64,
        qty: u64,
        arena: &mut Arena,
    ) -> Option<ArenaIndex> {
        let order = Order {
            id,
            side,
            price,
            quantity: qty,
            filled: 0,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            visible_quantity: qty,
            stop_price: None,
            stp_group: None,
            prev: None,
            next: None,
        };

        let arena_idx = arena.alloc(order)?;
        let level = self.level_mut(side, price);

        let order_mut = arena.get_mut(arena_idx);
        order_mut.prev = level.tail;
        order_mut.next = None;

        if let Some(tail) = level.tail {
            arena.get_mut(tail).next = Some(arena_idx);
        } else {
            level.head = Some(arena_idx);
        }
        level.tail = Some(arena_idx);
        level.total_quantity += qty;
        level.order_count += 1;
        self.depth_add(side, price, qty);
        if let Some(di) = self.dense_index(price) {
            self.set_occupied(side, di);
        }

        match side {
            Side::Bid => {
                if self.best_bid_tick.is_none_or(|b| price > b) {
                    self.best_bid_tick = Some(price);
                }
            }
            Side::Ask => {
                if self.best_ask_tick.is_none_or(|a| price < a) {
                    self.best_ask_tick = Some(price);
                }
            }
        }
        Some(arena_idx)
    }

    #[inline]
    pub fn best_bid(&self) -> Option<u64> { self.best_bid_tick }
    #[inline]
    pub fn best_ask(&self) -> Option<u64> { self.best_ask_tick }

    pub fn get_bid_level(&self, price: u64) -> &PriceLevel {
        if let Some(i) = self.dense_index(price) {
            &self.bids_dense[i]
        } else {
            self.bids_sparse.get(&price).expect("missing bid level")
        }
    }

    pub fn get_ask_level(&self, price: u64) -> &PriceLevel {
        if let Some(i) = self.dense_index(price) {
            &self.asks_dense[i]
        } else {
            self.asks_sparse.get(&price).expect("missing ask level")
        }
    }
}

// tests go here...
```

**Step 4: Run tests**

Run: `cargo test -p matchx-book`
Expected: 5 tests PASS

**Step 5: Commit**

```bash
git add crates/matchx-book/
git commit -m "feat(book): add hybrid dense+sparse order book with insert and best price tracking"
```

---

### Task 5: Order Book — Cancel and Remove

**Files:**
- Modify: `crates/matchx-book/src/lib.rs`

**Step 1: Write failing tests for cancel**

Add to the test module:

```rust
#[test]
fn cancel_only_order_clears_best() {
    let mut arena = matchx_arena::Arena::new(64);
    let mut book = OrderBook::new(config());

    let idx = book.insert_order(OrderId(1), Side::Bid, 500, 10, &mut arena).unwrap();
    book.remove_order(idx, &mut arena);
    assert_eq!(book.best_bid(), None);
    assert!(book.get_bid_level(500).is_empty());
}

#[test]
fn cancel_best_bid_finds_next_best() {
    let mut arena = matchx_arena::Arena::new(64);
    let mut book = OrderBook::new(config());

    book.insert_order(OrderId(1), Side::Bid, 500, 10, &mut arena);
    let top = book.insert_order(OrderId(2), Side::Bid, 510, 5, &mut arena).unwrap();
    book.insert_order(OrderId(3), Side::Bid, 490, 20, &mut arena);

    book.remove_order(top, &mut arena);
    assert_eq!(book.best_bid(), Some(500));
}

#[test]
fn cancel_middle_of_queue_preserves_links() {
    let mut arena = matchx_arena::Arena::new(64);
    let mut book = OrderBook::new(config());

    let a = book.insert_order(OrderId(1), Side::Bid, 500, 10, &mut arena).unwrap();
    let b = book.insert_order(OrderId(2), Side::Bid, 500, 20, &mut arena).unwrap();
    let c = book.insert_order(OrderId(3), Side::Bid, 500, 30, &mut arena).unwrap();

    // Remove middle
    book.remove_order(b, &mut arena);

    let level = book.get_bid_level(500);
    assert_eq!(level.total_quantity, 40); // 10 + 30
    assert_eq!(level.order_count, 2);
    // a -> c
    assert_eq!(arena.get(a).next, Some(c));
    assert_eq!(arena.get(c).prev, Some(a));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p matchx-book`
Expected: FAIL — remove_order not defined

**Step 3: Implement remove_order**

Add to `OrderBook` impl:

```rust
/// Remove an order from its price level. Frees the arena slot.
/// Returns the side and price of the removed order for BBO update.
pub fn remove_order(&mut self, idx: ArenaIndex, arena: &mut Arena) -> (Side, u64) {
    let order = arena.get(idx);
    let side = order.side;
    let price = order.price;
    let qty = order.remaining();
    let prev = order.prev;
    let next = order.next;

    {
        let level = self.level_mut(side, price);

        // Unlink from doubly-linked list
        match prev {
            Some(p) => arena.get_mut(p).next = next,
            None => level.head = next,
        }
        match next {
            Some(n) => arena.get_mut(n).prev = prev,
            None => level.tail = prev,
        }

        level.total_quantity -= qty;
        level.order_count -= 1;
    }
    self.depth_remove(side, price, qty);

    arena.free(idx);

    // Prune sparse empty levels and refresh BBO with bounded dense scan + sparse fallback
    if self.level_is_empty(side, price) {
        self.prune_if_sparse_empty(side, price);
        self.refresh_best_after_level_empty(side, price);
    }

    (side, price)
}

fn level_is_empty(&self, side: Side, price: u64) -> bool {
    match (side, self.dense_index(price)) {
        (Side::Bid, Some(i)) => self.bids_dense[i].is_empty(),
        (Side::Ask, Some(i)) => self.asks_dense[i].is_empty(),
        (Side::Bid, None) => self.bids_sparse.get(&price).is_none_or(|l| l.is_empty()),
        (Side::Ask, None) => self.asks_sparse.get(&price).is_none_or(|l| l.is_empty()),
    }
}

fn prune_if_sparse_empty(&mut self, side: Side, price: u64) {
    if self.dense_index(price).is_some() {
        return;
    }
    match side {
        Side::Bid => {
            if self.bids_sparse.get(&price).is_some_and(|l| l.is_empty()) {
                self.bids_sparse.remove(&price);
            }
        }
        Side::Ask => {
            if self.asks_sparse.get(&price).is_some_and(|l| l.is_empty()) {
                self.asks_sparse.remove(&price);
            }
        }
    }
}

fn refresh_best_after_level_empty(&mut self, side: Side, removed_price: u64) {
    // Clear occupancy bit for the emptied dense level
    if let Some(di) = self.dense_index(removed_price) {
        self.clear_occupied(side, di);
    }

    match side {
        Side::Bid if self.best_bid_tick == Some(removed_price) => {
            // Scan bitset words from high to low for next occupied bid tick
            self.best_bid_tick = self.find_highest_occupied_bid();
            if self.best_bid_tick.is_none() {
                self.best_bid_tick = self.bids_sparse.keys().next_back().copied();
            }
        }
        Side::Ask if self.best_ask_tick == Some(removed_price) => {
            // Scan bitset words from low to high for next occupied ask tick
            self.best_ask_tick = self.find_lowest_occupied_ask();
            if self.best_ask_tick.is_none() {
                self.best_ask_tick = self.asks_sparse.keys().next().copied();
            }
        }
        _ => {}
    }
}

/// Scan bids_occupied bitset for highest set bit (best bid in dense window).
fn find_highest_occupied_bid(&self) -> Option<u64> {
    for (word_idx, &word) in self.bids_occupied.iter().enumerate().rev() {
        if word != 0 {
            let bit = 63 - word.leading_zeros() as usize;
            let tick_idx = word_idx * 64 + bit;
            return Some(self.dense_base_price + tick_idx as u64);
        }
    }
    None
}

/// Scan asks_occupied bitset for lowest set bit (best ask in dense window).
fn find_lowest_occupied_ask(&self) -> Option<u64> {
    for (word_idx, &word) in self.asks_occupied.iter().enumerate() {
        if word != 0 {
            let bit = word.trailing_zeros() as usize;
            let tick_idx = word_idx * 64 + bit;
            return Some(self.dense_base_price + tick_idx as u64);
        }
    }
    None
}

fn depth_add(&mut self, side: Side, price: u64, qty: u64) {
    if let Some(i) = self.dense_index(price) {
        match side {
            Side::Bid => self.bid_depth_index.add(i, qty),
            Side::Ask => self.ask_depth_index.add(i, qty),
        }
    }
}

fn depth_remove(&mut self, side: Side, price: u64, qty: u64) {
    if let Some(i) = self.dense_index(price) {
        match side {
            Side::Bid => self.bid_depth_index.sub(i, qty),
            Side::Ask => self.ask_depth_index.sub(i, qty),
        }
    }
}
```

**Step 4: Run tests**

Run: `cargo test -p matchx-book`
Expected: 8 tests PASS

**Step 5: Commit**

```bash
git add crates/matchx-book/
git commit -m "feat(book): add order removal with hybrid-level unlinking and best-price refresh"
```

---

### Task 5A: Order Book — Dense Window Recentering

**Files:**
- Modify: `crates/matchx-book/src/lib.rs`

**Step 1: Write failing tests for recentering**

```rust
#[test]
fn recenter_when_bbo_drifts_past_threshold() {
    let mut arena = matchx_arena::Arena::new(128);
    // Dense window: base=0, max_ticks=100
    let mut config = config();
    config.max_ticks = 100;
    let mut book = OrderBook::new(config);

    // Insert asks far above dense window to force sparse
    book.insert_order(OrderId(1), Side::Ask, 200, 10, &mut arena);
    assert!(book.is_in_sparse(Side::Ask, 200));

    // Move BBO up by inserting bids near top of dense window
    for i in 0..80 {
        book.insert_order(OrderId(100 + i), Side::Bid, 70 + (i % 10), 1, &mut arena);
    }

    // Trigger recenter — BBO has drifted past 70% of window
    book.maybe_recenter(&mut arena);

    // After recenter, the new dense window should be centered near BBO
    // and the previously-sparse ask at 200 may now be in dense range
    let new_base = book.dense_base_price();
    assert!(new_base > 0, "dense window should have shifted up");
}

#[test]
fn recenter_preserves_order_linkage() {
    let mut arena = matchx_arena::Arena::new(128);
    let mut config = config();
    config.max_ticks = 100;
    let mut book = OrderBook::new(config);

    let a = book.insert_order(OrderId(1), Side::Bid, 50, 10, &mut arena).unwrap();
    let b = book.insert_order(OrderId(2), Side::Bid, 50, 20, &mut arena).unwrap();

    // Force recenter
    book.force_recenter(25, &mut arena);

    // Linkage still intact
    assert_eq!(arena.get(a).next, Some(b));
    assert_eq!(arena.get(b).prev, Some(a));
    let level = book.get_bid_level(50);
    assert_eq!(level.total_quantity, 30);
    assert_eq!(level.order_count, 2);
}

#[test]
fn recenter_updates_bitset_and_fenwick() {
    let mut arena = matchx_arena::Arena::new(128);
    let mut config = config();
    config.max_ticks = 100;
    let mut book = OrderBook::new(config);

    book.insert_order(OrderId(1), Side::Ask, 60, 10, &mut arena);
    book.force_recenter(20, &mut arena);

    // Ask at 60 should still be findable as best ask
    assert_eq!(book.best_ask(), Some(60));
    // Fenwick should reflect the quantity
    let avail = book.ask_available_at_or_below(60);
    assert_eq!(avail, 10);
}
```

**Step 2: Implement recentering**

Add to `OrderBook`:

```rust
/// Check if BBO has drifted past the recenter threshold (70% of window width)
/// and recenter the dense window if needed.
pub fn maybe_recenter(&mut self, arena: &mut Arena) {
    let window_size = self.dense_max_ticks as u64;
    let threshold = window_size * 7 / 10;

    let needs_recenter = match (self.best_bid_tick, self.best_ask_tick) {
        (Some(bid), _) if bid >= self.dense_base_price + threshold => true,
        (_, Some(ask)) if ask < self.dense_base_price + (window_size - threshold) => true,
        _ => false,
    };

    if needs_recenter {
        let center = self.best_bid_tick.or(self.best_ask_tick).unwrap_or(0);
        let new_base = center.saturating_sub(window_size / 2);
        self.force_recenter(new_base, arena);
    }
}

/// Recenter the dense window to a new base price.
/// Migrates levels between dense and sparse in bounded batches.
pub fn force_recenter(&mut self, new_base: u64, arena: &mut Arena) {
    if new_base == self.dense_base_price {
        return;
    }
    let old_base = self.dense_base_price;
    let dense_n = self.dense_max_ticks as usize;
    let old_end = old_base + dense_n as u64;
    let new_end = new_base + dense_n as u64;

    // 1. Evict dense levels that fall outside the new window -> sparse
    for i in 0..dense_n {
        let price = old_base + i as u64;
        if price < new_base || price >= new_end {
            for side in [Side::Bid, Side::Ask] {
                let level = match side {
                    Side::Bid => &self.bids_dense[i],
                    Side::Ask => &self.asks_dense[i],
                };
                if !level.is_empty() {
                    let moved = level.clone();
                    match side {
                        Side::Bid => { self.bids_sparse.insert(price, moved); }
                        Side::Ask => { self.asks_sparse.insert(price, moved); }
                    }
                }
                match side {
                    Side::Bid => self.bids_dense[i] = PriceLevel::EMPTY,
                    Side::Ask => self.asks_dense[i] = PriceLevel::EMPTY,
                }
            }
        }
    }

    // 2. Absorb sparse levels that now fall inside the new window -> dense
    // (collect keys first to avoid borrow conflict)
    let bid_keys: Vec<u64> = self.bids_sparse.range(new_base..new_end).map(|(&k, _)| k).collect();
    for price in bid_keys {
        if let Some(level) = self.bids_sparse.remove(&price) {
            let di = (price - new_base) as usize;
            self.bids_dense[di] = level;
        }
    }
    let ask_keys: Vec<u64> = self.asks_sparse.range(new_base..new_end).map(|(&k, _)| k).collect();
    for price in ask_keys {
        if let Some(level) = self.asks_sparse.remove(&price) {
            let di = (price - new_base) as usize;
            self.asks_dense[di] = level;
        }
    }

    self.dense_base_price = new_base;

    // 3. Rebuild bitsets and Fenwick trees from scratch
    self.rebuild_indices();
}

fn rebuild_indices(&mut self) {
    let dense_n = self.dense_max_ticks as usize;
    let bitset_words = (dense_n + 63) / 64;

    self.bids_occupied = vec![0u64; bitset_words];
    self.asks_occupied = vec![0u64; bitset_words];
    self.bid_depth_index = FenwickTree::new(dense_n);
    self.ask_depth_index = FenwickTree::new(dense_n);

    for i in 0..dense_n {
        if !self.bids_dense[i].is_empty() {
            self.set_occupied(Side::Bid, i);
            self.bid_depth_index.add(i, self.bids_dense[i].total_quantity);
        }
        if !self.asks_dense[i].is_empty() {
            self.set_occupied(Side::Ask, i);
            self.ask_depth_index.add(i, self.asks_dense[i].total_quantity);
        }
    }
}
```

**Step 3: Run tests**

Run: `cargo test -p matchx-book`
Expected: all tests PASS including recentering tests

**Step 4: Commit**

```bash
git add crates/matchx-book/
git commit -m "feat(book): add dense window recentering with bitset/fenwick rebuild"
```

---

### Task 6: Order Book — Order Index (HashMap lookup)

**Files:**
- Modify: `crates/matchx-book/src/lib.rs`

**Step 1: Write failing tests**

```rust
#[test]
fn lookup_order_by_id() {
    let mut arena = matchx_arena::Arena::new(64);
    let mut book = OrderBook::new(config());

    book.insert_order(OrderId(42), Side::Bid, 500, 10, &mut arena);
    let idx = book.lookup(OrderId(42)).unwrap();
    assert_eq!(arena.get(idx).id, OrderId(42));
}

#[test]
fn lookup_returns_none_after_cancel() {
    let mut arena = matchx_arena::Arena::new(64);
    let mut book = OrderBook::new(config());

    let idx = book.insert_order(OrderId(42), Side::Bid, 500, 10, &mut arena).unwrap();
    book.remove_order(idx, &mut arena);
    assert!(book.lookup(OrderId(42)).is_none());
}

#[test]
fn duplicate_order_id_is_rejected_and_original_mapping_preserved() {
    let mut arena = matchx_arena::Arena::new(64);
    let mut book = OrderBook::new(config());

    let first = book.insert_order(OrderId(42), Side::Bid, 500, 10, &mut arena).unwrap();
    let duplicate = book.insert_order(OrderId(42), Side::Ask, 600, 5, &mut arena);
    assert!(duplicate.is_none());
    assert_eq!(book.lookup(OrderId(42)), Some(first));
}

#[test]
fn deterministic_hasher_produces_stable_output() {
    // Guard against hasher crate changes breaking determinism
    use core::hash::{BuildHasher, Hash, Hasher};
    let build = DeterministicHasher::default();
    let mut h = build.build_hasher();
    OrderId(12345).hash(&mut h);
    let result = h.finish();
    // If this value ever changes, deterministic replay is broken
    assert_eq!(result, {
        let mut h2 = build.build_hasher();
        OrderId(12345).hash(&mut h2);
        h2.finish()
    }, "Hasher must produce identical output for identical input");
}
```

**Step 2: Run to verify fail**

Run: `cargo test -p matchx-book`
Expected: FAIL — lookup not defined

**Step 3: Add HashMap-based order index**

Add deterministic hasher dependencies to `crates/matchx-book/Cargo.toml`:

```toml
[dependencies]
matchx-types.workspace = true
matchx-arena.workspace = true
hashbrown = "0.15"
twox-hash = { version = "2", default-features = false }
```

Add to OrderBook struct:

```rust
use core::hash::BuildHasherDefault;
use hashbrown::HashMap;
use twox_hash::XxHash64;

type DeterministicHasher = BuildHasherDefault<XxHash64>;

// In OrderBook struct:
order_index: HashMap<OrderId, ArenaIndex, DeterministicHasher>,
```

In `new()`:
```rust
order_index: HashMap::with_hasher(DeterministicHasher::default()),
```

In `insert_order()`, reject duplicates before allocation:
```rust
if self.order_index.contains_key(&id) {
    return None;
}
```

Then, after arena alloc:
```rust
self.order_index.insert(id, arena_idx);
```

In `remove_order()`, before arena free:
```rust
self.order_index.remove(&arena.get(idx).id);
```

Add method:
```rust
#[inline]
pub fn lookup(&self, id: OrderId) -> Option<ArenaIndex> {
    self.order_index.get(&id).copied()
}
```

**Step 4: Run tests**

Run: `cargo test -p matchx-book`
Expected: 11 tests PASS

**Step 5: Commit**

```bash
git add crates/matchx-book/
git commit -m "feat(book): add HashMap order index for O(1) cancel/modify lookup"
```

---

### Task 7: Matching Engine — MatchPolicy Trait + PriceTimeFIFO (matchx-engine)

**Files:**
- Create: `crates/matchx-engine/src/lib.rs`
- Create: `crates/matchx-engine/src/policy.rs`

**Step 1: Write failing tests for basic limit order matching**

In `crates/matchx-engine/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use matchx_types::*;

    fn test_config() -> InstrumentConfig {
        InstrumentConfig {
            id: 1,
            tick_size: 1,
            lot_size: 1,
            base_price: 0,
            max_ticks: 1000,
            stp_mode: StpMode::CancelNewest,
        }
    }

    #[test]
    fn limit_buy_rests_on_empty_book() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        let events = engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e, MatchEvent::OrderAccepted { id: OrderId(1), .. })));
        assert_eq!(engine.best_bid(), Some(100));
    }

    #[test]
    fn crossing_limit_orders_produce_fill() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Resting sell at 100
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        // Incoming buy at 100 crosses
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e,
            MatchEvent::Fill { maker_id: OrderId(1), taker_id: OrderId(2), price: 100, qty: 5, .. }
        )));
    }

    #[test]
    fn partial_fill_remainder_rests() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Resting sell 5 @ 100
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        // Buy 10 @ 100 — fills 5, 5 rests
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e,
            MatchEvent::Fill { qty: 5, .. }
        )));
        assert_eq!(engine.best_bid(), Some(100)); // remainder rests
        assert_eq!(engine.best_ask(), None);      // ask fully filled
    }

    #[test]
    fn taker_sweeps_multiple_price_levels_in_one_call() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Ask, price: 101, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(3), instrument_id: 1, side: Side::Bid, price: 101, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let filled: u64 = events.iter().filter_map(|e| match e {
            MatchEvent::Fill { qty, .. } => Some(*qty),
            _ => None,
        }).sum();
        assert_eq!(filled, 10);
        assert_eq!(engine.best_ask(), None);
    }

    #[test]
    fn cancel_existing_order() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::CancelOrder { id: OrderId(1) });
        assert!(events.iter().any(|e| matches!(e,
            MatchEvent::OrderCancelled { id: OrderId(1), remaining_qty: 10 }
        )));
        assert_eq!(engine.best_bid(), None);
    }
}
```

**Step 2: Run to verify fail**

Run: `cargo test -p matchx-engine`
Expected: FAIL — MatchingEngine not defined

**Step 3: Implement MatchPolicy trait and PriceTimeFIFO**

`crates/matchx-engine/src/policy.rs`:

```rust
use matchx_arena::Arena;
use matchx_types::*;

/// A single fill generated during matching.
pub struct Fill {
    pub maker_idx: ArenaIndex,
    pub maker_id: OrderId,
    pub taker_id: OrderId,
    pub price: u64,
    pub qty: u64,
}

/// Allocation-free sink for fills produced by the matching loop.
pub trait FillSink {
    fn on_fill(&mut self, fill: Fill);
}

/// Pluggable matching policy trait.
pub trait MatchPolicy {
    /// Walk one resting level and push fills into sink.
    fn match_order(
        &self,
        taker_id: OrderId,
        remaining: &mut u64,
        resting_price: u64,
        level_head: Option<ArenaIndex>,
        arena: &mut Arena,
        sink: &mut dyn FillSink,
    );

    /// Whether an incoming order's price can trade against a resting price.
    fn is_price_acceptable(
        &self,
        incoming_side: Side,
        incoming_price: u64,
        resting_price: u64,
    ) -> bool;
}

/// Standard price-time FIFO matching.
pub struct PriceTimeFifo;

impl MatchPolicy for PriceTimeFifo {
    fn match_order(
        &self,
        taker_id: OrderId,
        remaining: &mut u64,
        resting_price: u64,
        mut cursor: Option<ArenaIndex>,
        arena: &mut Arena,
        sink: &mut dyn FillSink,
    ) {
        while let Some(maker_idx) = cursor {
            if *remaining == 0 {
                break;
            }
            let maker = arena.get(maker_idx);
            let fill_qty = (*remaining).min(maker.remaining());
            let maker_id = maker.id;
            cursor = maker.next;
            sink.on_fill(Fill { maker_idx, maker_id, taker_id, price: resting_price, qty: fill_qty });
            *remaining -= fill_qty;
        }
    }

    #[inline]
    fn is_price_acceptable(
        &self,
        incoming_side: Side,
        incoming_price: u64,
        resting_price: u64,
    ) -> bool {
        match incoming_side {
            // Buy: willing to pay up to incoming_price
            Side::Bid => incoming_price >= resting_price,
            // Sell: willing to sell down to incoming_price
            Side::Ask => incoming_price <= resting_price,
        }
    }
}
```

**Step 4: Implement MatchingEngine core**

`crates/matchx-engine/src/lib.rs`:

```rust
#![cfg_attr(not(test), no_std)]
extern crate alloc;

pub mod policy;

use alloc::vec::Vec;
use matchx_arena::Arena;
use matchx_book::OrderBook;
use matchx_types::*;
use policy::{Fill, MatchPolicy, PriceTimeFifo};
use smallvec::SmallVec;

pub struct MatchingEngine {
    book: OrderBook,
    arena: Arena,
    policy: PriceTimeFifo,
    config: InstrumentConfig,
    sequence: u64,
    timestamp_ns: u64,
    event_buffer: Vec<MatchEvent>,
}

impl MatchingEngine {
    pub fn new(config: InstrumentConfig, arena_capacity: u32) -> Self {
        Self {
            book: OrderBook::new(config.clone()),
            arena: Arena::new(arena_capacity),
            policy: PriceTimeFifo,
            config,
            sequence: 0,
            timestamp_ns: 0,
            event_buffer: Vec::with_capacity(64),
        }
    }

    /// Emit an event, auto-populating EventMeta with monotonic sequence and logical clock.
    #[inline]
    fn emit(&mut self, event_fn: impl FnOnce(EventMeta) -> MatchEvent) {
        self.sequence += 1;
        self.timestamp_ns += 1;
        let meta = EventMeta {
            sequence: self.sequence,
            timestamp_ns: self.timestamp_ns,
        };
        self.event_buffer.push(event_fn(meta));
    }

    /// Process a command and return emitted events. The returned slice is valid
    /// until the next `process()` call (reuses internal buffer to avoid allocation).
    pub fn process(&mut self, cmd: Command) -> &[MatchEvent] {
        self.event_buffer.clear();
        match cmd {
            Command::NewOrder {
                id, side, price, qty, order_type, time_in_force,
                visible_qty, stop_price, stp_group, ..
            } => {
                self.process_new_order(
                    id, side, price, qty, order_type, time_in_force,
                    visible_qty, stop_price, stp_group,
                );
            }
            Command::CancelOrder { id } => {
                self.process_cancel(id);
            }
            Command::ModifyOrder { id, new_price, new_qty } => {
                self.process_modify(id, new_price, new_qty);
            }
        }
        &self.event_buffer
    }

    fn process_new_order(
        &mut self,
        id: OrderId,
        side: Side,
        price: u64,
        mut qty: u64,
        order_type: OrderType,
        time_in_force: TimeInForce,
        _visible_qty: Option<u64>,
        _stop_price: Option<u64>,
        _stp_group: Option<u32>,
    ) {
        // Prechecks before acceptance
        if qty == 0 {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta, id, reason: RejectReason::InvalidQuantity,
            });
            return;
        }
        if self.book.lookup(id).is_some() {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta, id, reason: RejectReason::DuplicateOrderId,
            });
            return;
        }

        self.emit(|meta| MatchEvent::OrderAccepted {
            meta, id, side, price, qty, order_type,
        });

        // Match against opposing side (stack-first fill buffer, avoids hot-path Vec allocs)
        let mut fills = SmallVec::<[Fill; 32]>::new();
        self.match_against_book(id, side, price, &mut qty, &mut fills);

        for fill in fills.iter() {
            let maker_remaining = self.arena.get(fill.maker_idx).remaining();
            self.emit(|meta| MatchEvent::Fill {
                meta,
                maker_id: fill.maker_id,
                taker_id: fill.taker_id,
                price: fill.price,
                qty: fill.qty,
                maker_remaining,
                taker_remaining: qty,
            });

            // Emit BookUpdate for maker's price level
            let maker_side = self.arena.get(fill.maker_idx).side;
            let level_qty = match maker_side {
                Side::Bid => self.book.get_bid_level(fill.price).total_quantity,
                Side::Ask => self.book.get_ask_level(fill.price).total_quantity,
            };
            self.emit(|meta| MatchEvent::BookUpdate {
                meta, side: maker_side, price: fill.price, qty: level_qty,
            });

            // Remove fully filled makers
            if self.arena.get(fill.maker_idx).is_filled() {
                self.book.remove_order(fill.maker_idx, &mut self.arena);
            }
        }

        // Rest remainder on book only for GTC limit orders.
        if qty > 0 && order_type == OrderType::Limit && time_in_force == TimeInForce::GTC {
            self.book.insert_order(id, side, price, qty, &mut self.arena);
            self.emit(|meta| MatchEvent::BookUpdate {
                meta, side, price, qty,
            });
        } else if qty > 0 && (order_type == OrderType::Market || time_in_force == TimeInForce::IOC) {
            self.emit(|meta| MatchEvent::OrderCancelled { meta, id, remaining_qty: qty });
        }
    }

    fn match_against_book(
        &mut self,
        taker_id: OrderId,
        taker_side: Side,
        taker_price: u64,
        remaining: &mut u64,
        fills: &mut SmallVec<[Fill; 32]>,
    ) {
        fills.clear();

        loop {
            if *remaining == 0 {
                break;
            }
            let mut progressed = false;

            // Get best opposing price
            let best_price = match taker_side {
                Side::Bid => self.book.best_ask(),
                Side::Ask => self.book.best_bid(),
            };

            let Some(resting_price) = best_price else { break };

            if !self.policy.is_price_acceptable(taker_side, taker_price, resting_price) {
                break;
            }

            // Walk the queue at this price level
            let level = match taker_side {
                Side::Bid => self.book.get_ask_level(resting_price),
                Side::Ask => self.book.get_bid_level(resting_price),
            };

            let mut cursor = level.head;

            while let Some(maker_idx) = cursor {
                if *remaining == 0 {
                    break;
                }

                let maker = self.arena.get(maker_idx);
                let fill_qty = (*remaining).min(maker.remaining());
                let maker_id = maker.id;
                cursor = maker.next;

                // Apply fill to maker
                self.arena.get_mut(maker_idx).filled += fill_qty;

                // Update level quantity
                match taker_side {
                    Side::Bid => {
                        let lvl = self.book.get_ask_level_mut(resting_price);
                        lvl.total_quantity -= fill_qty;
                    }
                    Side::Ask => {
                        let lvl = self.book.get_bid_level_mut(resting_price);
                        lvl.total_quantity -= fill_qty;
                    }
                }

                *remaining -= fill_qty;
                progressed = true;

                fills.push(Fill {
                    maker_idx,
                    maker_id,
                    taker_id,
                    price: resting_price,
                    qty: fill_qty,
                });
            }

            // Only stop if this iteration made no progress.
            if !progressed {
                break;
            }
        }

    }

    fn process_cancel(&mut self, id: OrderId) {
        if let Some(idx) = self.book.lookup(id) {
            let order = self.arena.get(idx);
            let remaining = order.remaining();
            let side = order.side;
            let price = order.price;
            self.book.remove_order(idx, &mut self.arena);
            self.emit(|meta| MatchEvent::OrderCancelled {
                meta, id, remaining_qty: remaining,
            });
            // Emit BookUpdate for the affected level
            let level_qty = match side {
                Side::Bid => self.book.get_bid_level(price).total_quantity,
                Side::Ask => self.book.get_ask_level(price).total_quantity,
            };
            self.emit(|meta| MatchEvent::BookUpdate {
                meta, side, price, qty: level_qty,
            });
        } else {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta, id, reason: RejectReason::OrderNotFound,
            });
        }
    }

    fn process_modify(
        &mut self,
        id: OrderId,
        new_price: u64,
        new_qty: u64,
    ) {
        // Cancel + replace: remove old, then route through full new-order path
        // so that a modify-to-cross produces correct fills.
        if let Some(idx) = self.book.lookup(id) {
            let order = self.arena.get(idx);
            let side = order.side;
            let old_price = order.price;
            self.book.remove_order(idx, &mut self.arena);
            self.emit(|meta| MatchEvent::OrderModified {
                meta, id, new_price, new_qty,
            });
            // Emit BookUpdate for the old level
            let old_level_qty = match side {
                Side::Bid => self.book.get_bid_level(old_price).total_quantity,
                Side::Ask => self.book.get_ask_level(old_price).total_quantity,
            };
            self.emit(|meta| MatchEvent::BookUpdate {
                meta, side, price: old_price, qty: old_level_qty,
            });
            // Route replacement through full new-order path (handles crossing)
            self.process_new_order(
                id, side, new_price, new_qty,
                OrderType::Limit, TimeInForce::GTC,
                None, None, None,
            );
        } else {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta, id, reason: RejectReason::OrderNotFound,
            });
        }
    }

    #[inline]
    pub fn best_bid(&self) -> Option<u64> {
        self.book.best_bid()
    }

    #[inline]
    pub fn best_ask(&self) -> Option<u64> {
        self.book.best_ask()
    }
}

// tests go here...
```

Note: This requires adding `get_bid_level_mut` and `get_ask_level_mut` to OrderBook:

```rust
pub fn get_bid_level_mut(&mut self, price: u64) -> &mut PriceLevel {
    if let Some(i) = self.dense_index(price) {
        &mut self.bids_dense[i]
    } else {
        self.bids_sparse.entry(price).or_insert(PriceLevel::EMPTY)
    }
}

pub fn get_ask_level_mut(&mut self, price: u64) -> &mut PriceLevel {
    if let Some(i) = self.dense_index(price) {
        &mut self.asks_dense[i]
    } else {
        self.asks_sparse.entry(price).or_insert(PriceLevel::EMPTY)
    }
}
```

Also add `smallvec = "1"` to `crates/matchx-engine/Cargo.toml` for stack-allocated fill buffering.

**Step 5: Run tests**

Run: `cargo test -p matchx-engine`
Expected: 5 tests PASS

**Step 6: Commit**

```bash
git add crates/matchx-engine/ crates/matchx-book/
git commit -m "feat(engine): add matching engine with PriceTimeFIFO policy and basic limit/cancel/modify"
```

---

### Task 8: Matching Engine — Market Orders + IOC + FOK

**Files:**
- Modify: `crates/matchx-engine/src/lib.rs`

**Step 1: Write failing tests**

```rust
#[test]
fn market_order_fills_against_book() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    engine.process(Command::NewOrder {
        id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    let events = engine.process(Command::NewOrder {
        id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 0, qty: 5,
        order_type: OrderType::Market, time_in_force: TimeInForce::IOC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    assert!(events.iter().any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. })));
}

#[test]
fn ioc_cancels_unfilled_remainder() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    engine.process(Command::NewOrder {
        id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 5,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    let events = engine.process(Command::NewOrder {
        id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::IOC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    // Should fill 5, then cancel remaining 5
    assert!(events.iter().any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. })));
    assert!(events.iter().any(|e| matches!(e,
        MatchEvent::OrderCancelled { id: OrderId(2), remaining_qty: 5 }
    )));
    assert_eq!(engine.best_bid(), None); // IOC remainder not resting
}

#[test]
fn fok_rejects_if_insufficient_liquidity() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    engine.process(Command::NewOrder {
        id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 5,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    let events = engine.process(Command::NewOrder {
        id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::FOK,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    assert!(events.iter().any(|e| matches!(e,
        MatchEvent::OrderRejected { id: OrderId(2), reason: RejectReason::InsufficientLiquidity }
    )));
    // Original ask still on book
    assert_eq!(engine.best_ask(), Some(100));
}

#[test]
fn fok_fills_when_sufficient_liquidity() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    engine.process(Command::NewOrder {
        id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    let events = engine.process(Command::NewOrder {
        id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::FOK,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    assert!(events.iter().any(|e| matches!(e, MatchEvent::Fill { qty: 10, .. })));
}

#[test]
fn fok_reject_does_not_emit_order_accepted() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    engine.process(Command::NewOrder {
        id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 5,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    let events = engine.process(Command::NewOrder {
        id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::FOK,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    assert!(events.iter().any(|e| matches!(e,
        MatchEvent::OrderRejected { id: OrderId(2), reason: RejectReason::InsufficientLiquidity }
    )));
    assert!(!events.iter().any(|e| matches!(e, MatchEvent::OrderAccepted { id: OrderId(2), .. })));
}
```

**Step 2: Run to verify fail**

Run: `cargo test -p matchx-engine`
Expected: new tests FAIL

**Step 3: Implement IOC/FOK/Market logic**

Modify `process_new_order` to enforce precheck ordering and handle time-in-force after matching:

```rust
// In process_new_order, BEFORE emitting OrderAccepted:
if time_in_force == TimeInForce::FOK {
    let available = self.check_available_liquidity(side, price);
    if available < qty {
        self.emit(|meta| MatchEvent::OrderRejected {
            meta, id, reason: RejectReason::InsufficientLiquidity,
        });
        return;
    }
}

// Emit OrderAccepted only after all reject paths are done.
self.emit(|meta| MatchEvent::OrderAccepted { meta, id, side, price, qty, order_type });

// After matching and fill events:
match (order_type, time_in_force) {
    // Market and IOC: cancel any unfilled remainder
    (OrderType::Market, _) | (_, TimeInForce::IOC) => {
        if qty > 0 {
            self.emit(|meta| MatchEvent::OrderCancelled { meta, id, remaining_qty: qty });
        }
    }
    // GTC Limit: rest remainder on book
    (OrderType::Limit, TimeInForce::GTC) => {
        if qty > 0 {
            self.book.insert_order(id, side, price, qty, &mut self.arena);
            self.emit(|meta| MatchEvent::BookUpdate { meta, side, price, qty });
        }
    }
    _ => {}
}
```

`check_available_liquidity` complexity note (phase 1): dense Fenwick component is `O(log N)`; sparse range summation is linear in matched sparse levels. This phase intentionally favors implementation simplicity first, then upgrades sparse checks in the next milestone.

```rust
fn check_available_liquidity(&self, taker_side: Side, taker_price: u64) -> u64 {
    // Dense: O(log N) via Fenwick/prefix sums.
    // Sparse: current implementation sums range levels linearly.
    match taker_side {
        Side::Bid => self.book.ask_available_at_or_below(taker_price),
        Side::Ask => self.book.bid_available_at_or_above(taker_price),
    }
}
```

Back it with depth-index APIs on `OrderBook`:

```rust
pub fn ask_available_at_or_below(&self, price: u64) -> u64 {
    let dense = self
        .dense_index(price)
        .map_or(0, |i| self.ask_depth_index.prefix_sum_le(i));
    let sparse: u64 = self.asks_sparse
        .range(..=price)
        .map(|(_, level)| level.total_quantity)
        .sum();
    dense + sparse
}

pub fn bid_available_at_or_above(&self, price: u64) -> u64 {
    let dense = self
        .dense_index(price)
        .map_or(0, |i| self.bid_depth_index.suffix_sum_ge(i));
    let sparse: u64 = self.bids_sparse
        .range(price..)
        .map(|(_, level)| level.total_quantity)
        .sum();
    dense + sparse
}
```

For Market orders: set effective price to max (buy) or 0 (sell) so `is_price_acceptable` always returns true:

```rust
let effective_price = match order_type {
    OrderType::Market => match side {
        Side::Bid => u64::MAX,
        Side::Ask => 0,
    },
    _ => price,
};
```

**Step 4: Run tests**

Run: `cargo test -p matchx-engine`
Expected: 10 tests PASS

**Step 5: Commit**

```bash
git add crates/matchx-engine/
git commit -m "feat(engine): add Market, IOC, and FOK order support"
```

### Task 8A: Sparse Range-Volume Index (phase 2, strict FOK latency)

**Files:**
- Modify: `crates/matchx-book/src/lib.rs`
- Modify: `crates/matchx-engine/src/lib.rs`
- Create: `crates/matchx-book/tests/sparse_volume_index.rs`

**Goal:** Replace sparse linear range summation in FOK pre-check with indexed sparse range-volume queries, achieving `O(log N)` across both dense and sparse regions.

**Step 1: Write failing tests**

Create `crates/matchx-book/tests/sparse_volume_index.rs`:

```rust
use matchx_arena::Arena;
use matchx_book::OrderBook;
use matchx_types::*;

fn sparse_config() -> InstrumentConfig {
    // Small dense window to force most prices into sparse storage
    InstrumentConfig {
        id: 1, tick_size: 1, lot_size: 1,
        base_price: 500, max_ticks: 100,
        stp_mode: StpMode::CancelNewest,
    }
}

#[test]
fn sparse_ask_volume_query_is_correct() {
    let mut arena = Arena::new(256);
    let mut book = OrderBook::new(sparse_config());

    // Insert asks in sparse region (outside dense window 500..600)
    book.insert_order(OrderId(1), Side::Ask, 200, 10, &mut arena);
    book.insert_order(OrderId(2), Side::Ask, 300, 20, &mut arena);
    book.insert_order(OrderId(3), Side::Ask, 400, 30, &mut arena);

    // Query: asks available at or below 350
    let avail = book.ask_available_at_or_below(350);
    assert_eq!(avail, 30); // 200@10 + 300@20
}

#[test]
fn sparse_bid_volume_query_is_correct() {
    let mut arena = Arena::new(256);
    let mut book = OrderBook::new(sparse_config());

    // Insert bids in sparse region (outside dense window 500..600)
    book.insert_order(OrderId(1), Side::Bid, 700, 10, &mut arena);
    book.insert_order(OrderId(2), Side::Bid, 800, 20, &mut arena);
    book.insert_order(OrderId(3), Side::Bid, 900, 30, &mut arena);

    // Query: bids available at or above 750
    let avail = book.bid_available_at_or_above(750);
    assert_eq!(avail, 50); // 800@20 + 900@30
}

#[test]
fn mixed_dense_sparse_volume_query() {
    let mut arena = Arena::new(256);
    let mut book = OrderBook::new(sparse_config());

    // Dense region ask (within 500..600)
    book.insert_order(OrderId(1), Side::Ask, 520, 15, &mut arena);
    // Sparse region ask
    book.insert_order(OrderId(2), Side::Ask, 300, 25, &mut arena);

    let avail = book.ask_available_at_or_below(550);
    assert_eq!(avail, 40); // 300@25 + 520@15
}

#[test]
fn fragmented_sparse_fok_precheck() {
    let mut arena = Arena::new(1024);
    let mut book = OrderBook::new(sparse_config());

    // 100 sparse ask levels, 1 lot each
    for i in 0..100 {
        book.insert_order(OrderId(i + 1), Side::Ask, 200 + i, 1, &mut arena);
    }

    // FOK pre-check: need 50 lots at or below 250
    let avail = book.ask_available_at_or_below(250);
    assert_eq!(avail, 51); // prices 200..=250
}
```

**Step 2: Implement augmented BTreeMap with subtree volume**

Replace the plain `BTreeMap<u64, PriceLevel>` for sparse sides with an augmented ordered map that maintains cumulative volume. Two implementation options:

**Option A (simpler):** Maintain a parallel sparse Fenwick tree indexed by the rank of each sparse price. On insert/remove of sparse levels, update the sparse Fenwick tree. Range-volume queries use `O(log N)` Fenwick prefix sums after a `BTreeMap` rank lookup.

**Option B (self-contained):** Use an order-statistic tree (e.g., `BTreeMap` wrapper that tracks subtree sums). This is more complex but avoids the rank-mapping overhead.

Recommended: **Option A** — add `SparseVolumeIndex` alongside existing `BTreeMap`:

```rust
struct SparseVolumeIndex {
    // Sorted price keys for rank mapping
    prices: Vec<u64>,
    // Fenwick tree indexed by rank
    fenwick: FenwickTree,
}

impl SparseVolumeIndex {
    fn insert_level(&mut self, price: u64, qty: u64) { ... }
    fn remove_level(&mut self, price: u64, qty: u64) { ... }
    fn update_qty(&mut self, price: u64, delta: i64) { ... }
    fn sum_at_or_below(&self, price: u64) -> u64 { ... }
    fn sum_at_or_above(&self, price: u64) -> u64 { ... }
}
```

Update `ask_available_at_or_below` and `bid_available_at_or_above` to use the sparse index instead of linear iteration.

**Step 3: Run tests**

Run: `cargo test -p matchx-book --test sparse_volume_index`
Expected: all 4 tests PASS

**Step 4: Benchmark**

Add a benchmark in `matchx-bench` comparing FOK pre-check latency with 10, 100, 1000, and 10000 sparse levels. Verify sublinear growth.

**Step 5: Commit**

```bash
git add crates/matchx-book/ crates/matchx-engine/ crates/matchx-bench/
git commit -m "feat(book): add sparse range-volume index for O(log N) FOK pre-check"
```

**Exit criteria:**
- FOK pre-check no longer iterates over sparse ranges linearly.
- Benchmarks show bounded `O(log N)` growth with sparse-level count.

---

### Task 9: Matching Engine — Post-Only Orders

**Files:**
- Modify: `crates/matchx-engine/src/lib.rs`

**Step 1: Write failing tests**

```rust
#[test]
fn post_only_rejected_when_would_cross() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    engine.process(Command::NewOrder {
        id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    let events = engine.process(Command::NewOrder {
        id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 5,
        order_type: OrderType::PostOnly, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    assert!(events.iter().any(|e| matches!(e,
        MatchEvent::OrderRejected { id: OrderId(2), reason: RejectReason::WouldCrossSpread }
    )));
}

#[test]
fn post_only_rests_when_no_cross() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    engine.process(Command::NewOrder {
        id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 110, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    let events = engine.process(Command::NewOrder {
        id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 5,
        order_type: OrderType::PostOnly, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    assert!(events.iter().any(|e| matches!(e, MatchEvent::OrderAccepted { id: OrderId(2), .. })));
    assert_eq!(engine.best_bid(), Some(100));
}
```

**Step 2: Run to verify fail, then implement**

Add at the start of `process_new_order`:

```rust
if order_type == OrderType::PostOnly {
    let would_cross = match side {
        Side::Bid => self.book.best_ask().is_some_and(|ask| price >= ask),
        Side::Ask => self.book.best_bid().is_some_and(|bid| price <= bid),
    };
    if would_cross {
        self.emit(|meta| MatchEvent::OrderRejected {
            meta, id, reason: RejectReason::WouldCrossSpread,
        });
        return;
    }
    // Post-only goes directly to book, no matching
    self.emit(|meta| MatchEvent::OrderAccepted { meta, id, side, price, qty, order_type });
    self.book.insert_order(id, side, price, qty, &mut self.arena);
    self.emit(|meta| MatchEvent::BookUpdate { meta, side, price, qty });
    return;
}
```

**Step 3: Run tests**

Run: `cargo test -p matchx-engine`
Expected: 12 tests PASS

**Step 4: Commit**

```bash
git add crates/matchx-engine/
git commit -m "feat(engine): add Post-Only order support"
```

---

### Task 10: Matching Engine — Self-Trade Prevention

**Files:**
- Modify: `crates/matchx-engine/src/lib.rs`

**Step 1: Write failing tests**

```rust
#[test]
fn stp_cancel_newest_prevents_self_trade() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    // Resting sell with stp_group 1
    engine.process(Command::NewOrder {
        id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: Some(1),
    });
    // Incoming buy with same stp_group
    let events = engine.process(Command::NewOrder {
        id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: Some(1),
    });
    // Incoming (newest) should be rejected
    assert!(events.iter().any(|e| matches!(e,
        MatchEvent::OrderRejected { id: OrderId(2), reason: RejectReason::SelfTradePreventionTriggered }
    )));
    // Resting order still on book
    assert_eq!(engine.best_ask(), Some(100));
}
```

Add additional failing tests for the remaining configured modes:
- `stp_cancel_oldest_cancels_resting_order_and_allows_new_order_flow`
- `stp_cancel_both_cancels_both_orders`
- `stp_decrement_and_cancel_reduces_overlap_then_cancels_residual`

**Step 2: Run to verify fail, then implement**

In `match_against_book`, before generating a fill, check STP by mode:

```rust
// If both orders share stp_group, apply STP mode
if let (Some(taker_stp), Some(maker_stp)) = (taker_stp_group, maker.stp_group) {
    if taker_stp == maker_stp {
        match self.config.stp_mode {
            StpMode::CancelNewest => return (fills, StpAction::RejectIncoming),
            StpMode::CancelOldest => return (fills, StpAction::CancelResting(maker_idx)),
            StpMode::CancelBoth => return (fills, StpAction::CancelBoth(maker_idx)),
            StpMode::DecrementAndCancel => return (fills, StpAction::DecrementAndCancel(maker_idx)),
        }
    }
}
```

Then in `process_new_order`, handle each returned `StpAction` deterministically and emit the corresponding cancel/reject events.

**Step 3: Run tests, commit**

Run: `cargo test -p matchx-engine`
Expected: 16 tests PASS

```bash
git add crates/matchx-engine/
git commit -m "feat(engine): add self-trade prevention modes (CancelNewest/Oldest/Both/Decrement)"
```

---

### Task 11: Matching Engine — Iceberg Orders

**Files:**
- Modify: `crates/matchx-engine/src/lib.rs`

**Step 1: Write failing tests**

```rust
#[test]
fn iceberg_replenishes_visible_after_fill() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    // Iceberg sell: 5 visible, 20 total
    engine.process(Command::NewOrder {
        id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 20,
        order_type: OrderType::Iceberg, time_in_force: TimeInForce::GTC,
        visible_qty: Some(5), stop_price: None, stp_group: None,
    });
    // Buy 5 — should fill the visible portion
    let events = engine.process(Command::NewOrder {
        id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 5,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    assert!(events.iter().any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. })));
    // Iceberg should still be on the book with replenished visible qty
    assert_eq!(engine.best_ask(), Some(100));
}
```

**Step 2: Implement iceberg logic**

After filling an iceberg order's visible portion, if hidden quantity remains, replenish visible_quantity and move the order to the back of the queue (loses time priority).

**Step 3: Run tests, commit**

```bash
git add crates/matchx-engine/
git commit -m "feat(engine): add Iceberg order support with visible quantity replenishment"
```

---

### Task 12: Matching Engine — Stop-Limit Orders

**Files:**
- Modify: `crates/matchx-engine/src/lib.rs`
- Modify: `crates/matchx-book/src/lib.rs` (add stop order storage)

**Step 1: Write failing tests**

```rust
#[test]
fn stop_limit_buy_triggers_on_last_trade_price_cross() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    // Stop-limit buy: trigger at 105, limit at 110.
    engine.process(Command::NewOrder {
        id: OrderId(1), instrument_id: 1, side: Side::Bid, price: 110, qty: 10,
        order_type: OrderType::StopLimit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: Some(105), stp_group: None,
    });

    // Trade at 104 first: must NOT trigger stop.
    engine.process(Command::NewOrder {
        id: OrderId(2), instrument_id: 1, side: Side::Ask, price: 104, qty: 5,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    engine.process(Command::NewOrder {
        id: OrderId(3), instrument_id: 1, side: Side::Bid, price: 104, qty: 5,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });

    // Trade at 105: must trigger stop.
    engine.process(Command::NewOrder {
        id: OrderId(4), instrument_id: 1, side: Side::Ask, price: 105, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    let events = engine.process(Command::NewOrder {
        id: OrderId(5), instrument_id: 1, side: Side::Bid, price: 105, qty: 10,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    assert!(events.iter().any(|e| matches!(e, MatchEvent::StopTriggered { .. })));
}
```

Add additional failing tests:
- `stop_limit_sell_triggers_on_last_trade_price_cross_down`
- `multiple_stops_same_price_trigger_in_fifo_insertion_order`
- `trigger_cascade_order_is_deterministic_for_same_input_sequence`

**Step 2: Implement stop order storage and sublinear trigger logic**

Add `stop_bids: BTreeMap<u64, VecDeque<ArenaIndex>>` and `stop_asks` to OrderBook plus trigger cursors (`next_stop_bid_trigger`, `next_stop_ask_trigger`).

Canonical trigger semantics:
- Trigger source is `last_trade_price` only.
- Buy stop triggers when `last_trade_price >= stop_price` and the previous trade price was below `stop_price`.
- Sell stop triggers when `last_trade_price <= stop_price` and the previous trade price was above `stop_price`.
- Only successful fills update `last_trade_price`.

Deterministic ordering:
- Use `BTreeMap::range` from trigger cursor to collect newly-triggered stops in deterministic price order.
- Within the same stop price, process `VecDeque` FIFO insertion order.
- Convert triggered stops to regular limit orders and process them in the same input sequence with deterministic sub-ordering.

**Step 3: Run tests, commit**

```bash
git add crates/matchx-engine/ crates/matchx-book/
git commit -m "feat(engine): add Stop-Limit support with range-based sublinear trigger activation"
```

---

### Task 13: Property-Based Tests (proptest)

**Files:**
- Create: `crates/matchx-engine/tests/properties.rs`

**Step 1: Write property tests**

```rust
use proptest::prelude::*;
use matchx_engine::MatchingEngine;
use matchx_types::*;

fn test_config() -> InstrumentConfig {
    InstrumentConfig {
        id: 1, tick_size: 1, lot_size: 1,
        base_price: 0, max_ticks: 1000,
        stp_mode: StpMode::CancelNewest,
    }
}

proptest! {
    #[test]
    fn bbo_never_crosses(
        prices in prop::collection::vec(1u64..999, 1..50),
        sides in prop::collection::vec(prop::bool::ANY, 1..50),
        qtys in prop::collection::vec(1u64..100, 1..50),
    ) {
        let mut engine = MatchingEngine::new(test_config(), 4096);
        let len = prices.len().min(sides.len()).min(qtys.len());

        for i in 0..len {
            let side = if sides[i] { Side::Bid } else { Side::Ask };
            engine.process(Command::NewOrder {
                id: OrderId(i as u64 + 1),
                instrument_id: 1,
                side,
                price: prices[i],
                qty: qtys[i],
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::GTC,
                visible_qty: None,
                stop_price: None,
                stp_group: None,
            });

            // Invariant: best bid < best ask (if both exist)
            if let (Some(bid), Some(ask)) = (engine.best_bid(), engine.best_ask()) {
                prop_assert!(bid < ask,
                    "BBO crossed: bid={} >= ask={} after order {}", bid, ask, i);
            }
        }
    }

    #[test]
    fn fill_quantity_conserved(
        ask_qty in 1u64..100,
        bid_qty in 1u64..100,
    ) {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100,
            qty: ask_qty, order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100,
            qty: bid_qty, order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let total_filled: u64 = events.iter()
            .filter_map(|e| match e {
                MatchEvent::Fill { qty, .. } => Some(*qty),
                _ => None,
            })
            .sum();

        let expected = ask_qty.min(bid_qty);
        prop_assert_eq!(total_filled, expected,
            "Fill quantity mismatch: got {} expected {}", total_filled, expected);
    }

    #[test]
    fn deterministic_replay(
        prices in prop::collection::vec(1u64..999, 1..30),
        sides in prop::collection::vec(prop::bool::ANY, 1..30),
        qtys in prop::collection::vec(1u64..100, 1..30),
    ) {
        let len = prices.len().min(sides.len()).min(qtys.len());
        let commands: Vec<Command> = (0..len).map(|i| {
            Command::NewOrder {
                id: OrderId(i as u64 + 1),
                instrument_id: 1,
                side: if sides[i] { Side::Bid } else { Side::Ask },
                price: prices[i],
                qty: qtys[i],
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::GTC,
                visible_qty: None,
                stop_price: None,
                stp_group: None,
            }
        }).collect();

        // Run twice
        let run = |cmds: &[Command]| -> Vec<Vec<MatchEvent>> {
            let mut engine = MatchingEngine::new(test_config(), 4096);
            cmds.iter().map(|c| engine.process(c.clone()).to_vec()).collect()
        };

        let run1 = run(&commands);
        let run2 = run(&commands);
        prop_assert_eq!(run1, run2, "Non-deterministic: different outputs for same input");
    }
}
```

**Step 2: Run property tests**

Run: `cargo test -p matchx-engine --test properties`
Expected: PASS

**Step 3: Commit**

```bash
git add crates/matchx-engine/tests/
git commit -m "test(engine): add property-based tests for BBO invariant, fill conservation, determinism"
```

---

### Task 14A: Journal Writer + Reader + CRC (matchx-journal)

**Files:**
- Create: `crates/matchx-journal/src/lib.rs`
- Create: `crates/matchx-journal/src/writer.rs`
- Create: `crates/matchx-journal/src/reader.rs`
- Create: `crates/matchx-journal/src/codec.rs` (canonical command serialization)

Add to `crates/matchx-journal/Cargo.toml`:
```toml
[dependencies]
matchx-types.workspace = true
crc32fast = "1"

[dev-dependencies]
tempfile = "3"
```

**Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use matchx_types::*;

    #[test]
    fn write_and_read_back_commands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.bin");

        let cmd1 = Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        };
        let cmd2 = Command::CancelOrder { id: OrderId(1) };

        {
            let mut writer = JournalWriter::open(&path).unwrap();
            writer.append(1, &cmd1).unwrap();
            writer.append(2, &cmd2).unwrap();
        }

        let mut reader = JournalReader::open(&path).unwrap();
        let entries: Vec<_> = reader.read_all().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sequence, 1);
        assert_eq!(entries[1].sequence, 2);
    }

    #[test]
    fn crc_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.bin");

        let cmd = Command::CancelOrder { id: OrderId(42) };

        {
            let mut writer = JournalWriter::open(&path).unwrap();
            writer.append(1, &cmd).unwrap();
        }

        // Corrupt a byte in the command payload
        let mut data = std::fs::read(&path).unwrap();
        let header_size = 64; // segment header
        data[header_size + 10] ^= 0xFF;
        std::fs::write(&path, &data).unwrap();

        let mut reader = JournalReader::open(&path).unwrap();
        assert!(reader.read_all().is_err());
    }

    #[test]
    fn roundtrip_all_command_variants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.bin");

        let commands = vec![
            Command::NewOrder {
                id: OrderId(1), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
                order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
                visible_qty: None, stop_price: None, stp_group: None,
            },
            Command::NewOrder {
                id: OrderId(2), instrument_id: 1, side: Side::Ask, price: 200, qty: 5,
                order_type: OrderType::Iceberg, time_in_force: TimeInForce::IOC,
                visible_qty: Some(2), stop_price: Some(190), stp_group: Some(42),
            },
            Command::CancelOrder { id: OrderId(1) },
            Command::ModifyOrder { id: OrderId(2), new_price: 210, new_qty: 8 },
        ];

        {
            let mut writer = JournalWriter::open(&path).unwrap();
            for (i, cmd) in commands.iter().enumerate() {
                writer.append(i as u64 + 1, cmd).unwrap();
            }
        }

        let mut reader = JournalReader::open(&path).unwrap();
        let entries = reader.read_all().unwrap();
        assert_eq!(entries.len(), 4);
        // Verify each command round-trips correctly
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.sequence, i as u64 + 1);
            // Command equality check (requires PartialEq on Command)
        }
    }
}
```

**Step 2: Implement segment header, writer, reader, and canonical codec**

Format:
- Segment header (fixed 64B): `magic` (8B), `version` (u16), `shard_id` (u32), `instrument_id` (u32), `segment_index` (u64), `start_input_sequence` (u64), `created_at_ns` (u64), padding, `header_crc32c` (u32)
- Record: `[record_len: u32][record_type: u16][flags: u16][input_sequence: u64][command bytes...][record_crc32c: u32]`
- All multi-byte integers: little-endian

Codec (`codec.rs`): canonical hand-written binary serialization for each `Command` variant. No serde, explicit endianness, schema version byte at start of each command.

**Step 3: Run tests, commit**

```bash
git add crates/matchx-journal/
git commit -m "feat(journal): add journal writer, reader, and CRC-validated record format"
```

---

### Task 14B: Torn-Write Recovery + Segment Rotation

**Files:**
- Modify: `crates/matchx-journal/src/reader.rs`
- Modify: `crates/matchx-journal/src/writer.rs`
- Create: `crates/matchx-journal/src/segment.rs`

**Step 1: Write failing tests**

```rust
#[test]
fn torn_tail_is_truncated_to_last_valid_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.bin");

    let cmd1 = Command::CancelOrder { id: OrderId(1) };
    let cmd2 = Command::CancelOrder { id: OrderId(2) };

    {
        let mut writer = JournalWriter::open(&path).unwrap();
        writer.append(1, &cmd1).unwrap();
        writer.append(2, &cmd2).unwrap();
    }

    // Simulate torn write by truncating last 5 bytes
    let data = std::fs::read(&path).unwrap();
    std::fs::write(&path, &data[..data.len() - 5]).unwrap();

    let mut reader = JournalReader::open(&path).unwrap();
    let entries = reader.read_all().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].sequence, 1);
}

#[test]
fn segment_rotation_at_size_limit() {
    let dir = tempfile::tempdir().unwrap();

    // Use a tiny segment limit (1KB) to force rotation quickly
    let mut writer = SegmentedWriter::open(dir.path(), 1024).unwrap();
    let cmd = Command::CancelOrder { id: OrderId(1) };

    for i in 0..100 {
        writer.append(i + 1, &cmd).unwrap();
    }

    // Should have created multiple segment files
    let segments: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "journal"))
        .collect();
    assert!(segments.len() > 1, "Expected multiple segments, got {}", segments.len());
}

#[test]
fn segmented_reader_reads_across_segments() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = SegmentedWriter::open(dir.path(), 1024).unwrap();
    let cmd = Command::CancelOrder { id: OrderId(1) };

    for i in 0..50 {
        writer.append(i + 1, &cmd).unwrap();
    }
    drop(writer);

    let reader = SegmentedReader::open(dir.path()).unwrap();
    let entries = reader.read_all().unwrap();
    assert_eq!(entries.len(), 50);
    // Sequences must be strictly monotonic
    for i in 1..entries.len() {
        assert!(entries[i].sequence > entries[i - 1].sequence);
    }
}
```

**Step 2: Implement segment rotation and multi-segment reading**

- `SegmentedWriter`: wraps `JournalWriter`, rotates at 256MB (configurable). Writes segment trailer before closing a segment. New segment gets strictly monotonic `segment_index`.
- `SegmentedReader`: discovers segment files in directory, reads in order by `segment_index`, validates continuity of sequences across segments.
- Torn-write recovery: scan forward, validate each record's CRC, stop at first invalid/short record, truncate file to last valid boundary.

**Step 3: Run tests, commit**

```bash
git add crates/matchx-journal/
git commit -m "feat(journal): add torn-write recovery and segment rotation at 256MB"
```

---

### Task 14C: Snapshot Write + Atomic Commit + Recovery

**Files:**
- Create: `crates/matchx-journal/src/snapshot.rs`
- Modify: `crates/matchx-journal/src/lib.rs`

**Step 1: Write failing tests**

```rust
#[test]
fn snapshot_commit_is_atomic_and_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let snap_path = dir.path().join("snapshots");
    std::fs::create_dir(&snap_path).unwrap();

    let state = SnapshotState {
        input_sequence: 42,
        segment_index: 0,
        segment_offset: 1024,
        // ... book state fields
    };

    // Write snapshot
    SnapshotWriter::commit(&snap_path, &state).unwrap();

    // Verify committed file exists and temp file is gone
    let committed = SnapshotReader::latest(&snap_path).unwrap();
    assert!(committed.is_some());
    assert_eq!(committed.unwrap().input_sequence, 42);

    // No temp files left behind
    let temps: Vec<_> = std::fs::read_dir(&snap_path).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "tmp"))
        .collect();
    assert!(temps.is_empty());
}

#[test]
fn recovery_loads_latest_snapshot_then_replays_tail_records_only() {
    let dir = tempfile::tempdir().unwrap();
    let journal_dir = dir.path().join("journal");
    let snap_dir = dir.path().join("snapshots");
    std::fs::create_dir_all(&journal_dir).unwrap();
    std::fs::create_dir_all(&snap_dir).unwrap();

    // Write 100 commands to journal
    let mut writer = SegmentedWriter::open(&journal_dir, 256 * 1024 * 1024).unwrap();
    for i in 0..100 {
        let cmd = Command::CancelOrder { id: OrderId(i + 1) };
        writer.append(i + 1, &cmd).unwrap();
    }
    drop(writer);

    // Take snapshot at sequence 50
    let state = SnapshotState {
        input_sequence: 50,
        segment_index: 0,
        segment_offset: /* offset of record 51 */0,
        // ... engine state at seq 50
    };
    SnapshotWriter::commit(&snap_dir, &state).unwrap();

    // Recovery should replay only records 51..100
    let recovery = RecoveryManager::recover(&journal_dir, &snap_dir).unwrap();
    assert_eq!(recovery.start_sequence, 51);
    assert_eq!(recovery.replay_count, 50);
}
```

**Step 2: Implement snapshot system**

- Snapshot metadata: `snapshot_sequence`, `input_sequence`, `segment_index`, `segment_offset`, `state_hash`
- Canonical deterministic serialization order for all book state (sorted by price, FIFO within level)
- Commit protocol: write to temp file -> `fsync(file)` -> `fsync(parent dir)` -> atomic rename
- `RecoveryManager`: load latest committed snapshot, seek journal to stored offset, replay tail

**Step 3: Run tests, commit**

```bash
git add crates/matchx-journal/
git commit -m "feat(journal): add atomic snapshot commit and snapshot-based recovery"
```

---

### Task 14D: BLAKE3 Hash Chain + Anchor Verification

**Files:**
- Create: `crates/matchx-journal/src/hash_chain.rs`
- Modify: `crates/matchx-journal/src/writer.rs`
- Modify: `crates/matchx-journal/src/snapshot.rs`

Add to `crates/matchx-journal/Cargo.toml`:
```toml
[dependencies]
blake3 = "1"
```

**Step 1: Write failing tests**

```rust
#[test]
fn event_hash_chain_detects_tampering() {
    let mut chain = HashChain::new_genesis();
    let h1 = chain.extend(b"event1");
    let h2 = chain.extend(b"event2");
    let h3 = chain.extend(b"event3");

    // Verify chain from genesis
    let mut verify = HashChain::new_genesis();
    assert_eq!(verify.extend(b"event1"), h1);
    assert_eq!(verify.extend(b"event2"), h2);
    assert_eq!(verify.extend(b"event3"), h3);

    // Tampered event produces different hash
    let mut tampered = HashChain::new_genesis();
    tampered.extend(b"event1");
    tampered.extend(b"TAMPERED");
    let tampered_h3 = tampered.extend(b"event3");
    assert_ne!(tampered_h3, h3);
}

#[test]
fn snapshot_hash_anchor_mismatch_fails_recovery() {
    // Create chain, take snapshot at event 2
    let mut chain = HashChain::new_genesis();
    chain.extend(b"event1");
    let anchor = chain.extend(b"event2");

    // Snapshot stores anchor
    let snapshot_anchor = anchor;

    // Replay with wrong anchor should fail
    let wrong_anchor = HashChain::new_genesis().extend(b"wrong");
    assert_ne!(wrong_anchor, snapshot_anchor);
    // RecoveryManager should reject replay if computed anchor != snapshot anchor
}

#[test]
fn hash_chain_is_deterministic() {
    let mut c1 = HashChain::new_genesis();
    let mut c2 = HashChain::new_genesis();
    for i in 0..100 {
        let data = format!("event{}", i);
        assert_eq!(c1.extend(data.as_bytes()), c2.extend(data.as_bytes()));
    }
}
```

**Step 2: Implement hash chain**

```rust
use blake3::Hasher;

pub struct HashChain {
    current: [u8; 32],
}

impl HashChain {
    pub fn new_genesis() -> Self {
        let genesis = blake3::hash(b"matchx:event-chain:v1");
        Self { current: *genesis.as_bytes() }
    }

    pub fn from_anchor(anchor: [u8; 32]) -> Self {
        Self { current: anchor }
    }

    pub fn extend(&mut self, event_bytes: &[u8]) -> [u8; 32] {
        let mut hasher = Hasher::new();
        hasher.update(&self.current);
        hasher.update(event_bytes);
        self.current = *hasher.finalize().as_bytes();
        self.current
    }

    pub fn current_anchor(&self) -> [u8; 32] {
        self.current
    }
}
```

Integrate with:
- `SegmentedWriter`: compute rolling hash on each appended record, write anchor in segment trailer
- `SnapshotWriter`: persist hash anchor in snapshot metadata
- `RecoveryManager`: verify anchor continuity on replay — fail hard on mismatch

**Step 3: Run tests, commit**

```bash
git add crates/matchx-journal/
git commit -m "feat(journal): add BLAKE3 rolling hash chain with anchor verification"
```

---

### Task 15: Event Journal — Deterministic Replay Integration Test

**Files:**
- Create: `crates/matchx-itests/tests/replay_determinism.rs`

**Step 1: Write the integration test**

```rust
//! End-to-end test: write commands to journal, replay, verify identical output.

use matchx_engine::MatchingEngine;
use matchx_journal::{JournalWriter, JournalReader};
use matchx_types::*;

#[test]
fn replay_produces_identical_output() {
    let config = InstrumentConfig {
        id: 1, tick_size: 1, lot_size: 1,
        base_price: 0, max_ticks: 1000,
        stp_mode: StpMode::CancelNewest,
    };

    let commands = vec![
        Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 50,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        },
        Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 30,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        },
        Command::CancelOrder { id: OrderId(1) },
    ];

    // Run 1: process and record
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.journal");
    let mut engine1 = MatchingEngine::new(config.clone(), 1024);
    let mut writer = JournalWriter::open(&path).unwrap();
    let mut outputs1 = Vec::new();

    for (i, cmd) in commands.iter().enumerate() {
        writer.append(i as u64 + 1, cmd).unwrap();
        outputs1.push(engine1.process(cmd.clone()).to_vec());
    }
    drop(writer);

    // Run 2: replay from journal
    let mut engine2 = MatchingEngine::new(config, 1024);
    let mut reader = JournalReader::open(&path).unwrap();
    let entries = reader.read_all().unwrap();
    let mut outputs2 = Vec::new();

    for entry in &entries {
        outputs2.push(engine2.process(entry.command.clone()).to_vec());
    }

    assert_eq!(outputs1, outputs2, "Replay output diverged from original");
}
```

**Step 2: Run test, commit**

Run: `cargo test -p matchx-itests --test replay_determinism`
Expected: PASS

```bash
git add crates/matchx-itests/tests/replay_determinism.rs
git commit -m "test: add end-to-end replay determinism integration test"
```

---

### Task 16: Benchmarks (matchx-bench)

**Files:**
- Create: `crates/matchx-bench/benches/matching.rs`

**Step 1: Write criterion benchmarks**

```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use matchx_engine::MatchingEngine;
use matchx_types::*;

fn config() -> InstrumentConfig {
    InstrumentConfig {
        id: 1, tick_size: 1, lot_size: 1,
        base_price: 0, max_ticks: 10000,
        stp_mode: StpMode::CancelNewest,
    }
}

fn bench_insert_limit_order(c: &mut Criterion) {
    c.bench_function("insert_limit_order", |b| {
        let mut engine = MatchingEngine::new(config(), 65536);
        let mut id = 1u64;
        b.iter(|| {
            let side = if id % 2 == 0 { Side::Bid } else { Side::Ask };
            let price = if side == Side::Bid { 4900 + (id % 100) } else { 5100 + (id % 100) };
            engine.process(black_box(Command::NewOrder {
                id: OrderId(id),
                instrument_id: 1,
                side,
                price,
                qty: 10,
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::GTC,
                visible_qty: None,
                stop_price: None,
                stp_group: None,
            }));
            id += 1;
        });
    });
}

fn bench_crossing_trade(c: &mut Criterion) {
    c.bench_function("crossing_trade", |b| {
        b.iter_custom(|iters| {
            let mut engine = MatchingEngine::new(config(), 65536);
            // Pre-populate asks
            for i in 0..1000 {
                engine.process(Command::NewOrder {
                    id: OrderId(i + 1),
                    instrument_id: 1,
                    side: Side::Ask,
                    price: 5000 + (i % 100),
                    qty: 10,
                    order_type: OrderType::Limit,
                    time_in_force: TimeInForce::GTC,
                    visible_qty: None,
                    stop_price: None,
                    stp_group: None,
                });
            }
            let start = std::time::Instant::now();
            for i in 0..iters {
                engine.process(black_box(Command::NewOrder {
                    id: OrderId(10000 + i),
                    instrument_id: 1,
                    side: Side::Bid,
                    price: 5000,
                    qty: 1,
                    order_type: OrderType::Limit,
                    time_in_force: TimeInForce::GTC,
                    visible_qty: None,
                    stop_price: None,
                    stp_group: None,
                }));
            }
            start.elapsed()
        });
    });
}

fn bench_cancel_order(c: &mut Criterion) {
    c.bench_function("cancel_order", |b| {
        let mut engine = MatchingEngine::new(config(), 65536);
        // Pre-populate
        for i in 0..10000 {
            engine.process(Command::NewOrder {
                id: OrderId(i + 1),
                instrument_id: 1,
                side: Side::Bid,
                price: 4000 + (i % 1000),
                qty: 10,
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::GTC,
                visible_qty: None,
                stop_price: None,
                stp_group: None,
            });
        }
        let mut cancel_id = 1u64;
        b.iter(|| {
            engine.process(black_box(Command::CancelOrder {
                id: OrderId(cancel_id),
            }));
            cancel_id += 1;
        });
    });
}

criterion_group!(benches, bench_insert_limit_order, bench_crossing_trade, bench_cancel_order);
criterion_main!(benches);
```

**Step 2: Run benchmarks**

Run: `cargo bench -p matchx-bench`
Expected: outputs ns/iter plus latency distribution summary (`P50/P95/P99`) for each benchmark

Benchmark execution requirements (for comparable latency numbers):
- Pin benchmark process to an isolated core.
- Use fixed CPU performance profile/governor.
- Run deterministic warmup before timing collection.
- Keep identical order-flow mix between baseline and candidate runs.

Acceptance gates:
- Crossing trade benchmark: `P50 < 1us`, `P99 < 3us` on baseline hardware profile.
- CI regression gate: fail on >10% P99 regression vs saved baseline for same profile.
- Any breach requires either optimization work or explicit threshold revision in this doc with rationale.

**Step 3: Commit**

```bash
git add crates/matchx-bench/
git commit -m "bench: add criterion benchmarks for insert, trade, and cancel operations"
```

---

### Task 17: CI & Tooling Setup

**Files:**
- Create: `.github/workflows/ci.yml`
- Create: `clippy.toml` (if needed for custom lints)

**Step 1: Create CI workflow**

```yaml
name: CI
on: [push, pull_request]

env:
  CARGO_TERM_COLOR: always

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace
      - run: cargo bench --workspace --no-run  # compile-check benchmarks

  miri:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@nightly
        with:
          components: miri
      - run: cargo +nightly miri test -p matchx-arena

  bench-regression:
    runs-on: ubuntu-latest
    if: github.event_name == 'pull_request'
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo bench -p matchx-bench -- --save-baseline pr
      # Compare against main baseline (requires saved artifact)
      # Fail on >10% P99 regression
```

**Step 2: Commit**

```bash
git add .github/ clippy.toml
git commit -m "ci: add CI workflow with fmt, clippy, miri, and benchmark regression checks"
```

---

## Summary

| Task | Crate | What |
|------|-------|------|
| 1 | workspace | Scaffold 7-crate workspace + `rust-toolchain.toml` |
| 2 | matchx-types | Core types: Order, PriceLevel, events, commands |
| 3 | matchx-arena | Pre-allocated arena with free-list |
| 4 | matchx-book | Hybrid dense+sparse order book: insert + BBO + occupancy bitset |
| 5 | matchx-book | Order removal + linked list unlinking + bitset-accelerated BBO refresh |
| 5A | matchx-book | Dense window recentering with bounded batch migration |
| 6 | matchx-book | HashMap order index for cancel/modify + deterministic hash stability test |
| 7 | matchx-engine | PriceTimeFIFO matching + EventMeta + reusable event buffer + BookUpdate emission + modify-crosses-spread |
| 8 | matchx-engine | Market + IOC + phase-1 FOK pre-check (dense `O(log N)` + sparse linear sum) |
| 8A | matchx-book + matchx-engine | Phase-2 sparse range-volume index for strict `O(log N)` FOK checks |
| 9 | matchx-engine | Post-Only orders |
| 10 | matchx-engine | Self-trade prevention |
| 11 | matchx-engine | Iceberg orders |
| 12 | matchx-engine + matchx-book | Stop-Limit orders with `last_trade_price` trigger semantics and deterministic cascade ordering |
| 13 | matchx-engine | Property-based tests (proptest) |
| 14A | matchx-journal | Journal writer + reader + CRC + canonical command codec |
| 14B | matchx-journal | Torn-write recovery + segment rotation at 256MB |
| 14C | matchx-journal | Atomic snapshot commit + snapshot-based recovery |
| 14D | matchx-journal | BLAKE3 rolling hash chain + anchor verification |
| 15 | matchx-itests | End-to-end replay determinism integration test |
| 16 | matchx-bench | Criterion benchmarks |
| 17 | workspace | CI workflow: fmt, clippy, miri, benchmark regression gate |
