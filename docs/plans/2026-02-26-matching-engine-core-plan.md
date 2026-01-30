# Matching Engine Core Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a sub-microsecond crypto exchange matching engine core with tick-array order book, arena allocator, pluggable matching rules, and deterministic event sourcing.

**Architecture:** Cargo workspace with 6 crates: types, arena, book, engine, journal, bench. Single-threaded deterministic event loop. Tick-indexed arrays for O(1) price level access. Arena-allocated orders with intrusive linked lists. All output is an event stream replayable for determinism verification.

**Tech Stack:** Rust (stable), no_std where possible, proptest, criterion, cargo-fuzz

**Design doc:** `docs/plans/2026-02-26-matching-engine-core-design.md`

---

### Task 1: Workspace Scaffold

**Files:**
- Create: `Cargo.toml` (workspace root)
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

**Step 1: Create workspace Cargo.toml**

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
proptest = "1"
```

**Step 2: Create each crate Cargo.toml and empty lib.rs**

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

Each `src/lib.rs` starts as empty (or `// TODO`).

**Step 3: Verify workspace compiles**

Run: `cargo check`
Expected: compiles with no errors

**Step 4: Commit**

```bash
git add -A
git commit -m "feat: scaffold cargo workspace with 6 crates"
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
        self.quantity - self.filled
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

/// Events emitted by the matching engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchEvent {
    OrderAccepted {
        id: OrderId,
        side: Side,
        price: u64,
        qty: u64,
        order_type: OrderType,
    },
    OrderRejected {
        id: OrderId,
        reason: RejectReason,
    },
    Fill {
        maker_id: OrderId,
        taker_id: OrderId,
        price: u64,
        qty: u64,
        maker_remaining: u64,
        taker_remaining: u64,
    },
    OrderCancelled {
        id: OrderId,
        remaining_qty: u64,
    },
    OrderModified {
        id: OrderId,
        new_price: u64,
        new_qty: u64,
    },
    BookUpdate {
        side: Side,
        price: u64,
        qty: u64,       // new total at this level (0 = level removed)
    },
    StopTriggered {
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

**Step 4: Run tests to verify they pass**

Run: `cargo test -p matchx-types`
Expected: 3 tests PASS

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
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p matchx-book`
Expected: FAIL — OrderBook not defined

**Step 3: Implement OrderBook with insert + best price tracking**

```rust
#![cfg_attr(not(test), no_std)]
extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use matchx_arena::Arena;
use matchx_types::*;

/// Tick-array order book. O(1) price level access.
pub struct OrderBook {
    pub instrument_id: u32,
    bids: Vec<PriceLevel>,
    asks: Vec<PriceLevel>,
    base_price: u64,
    max_ticks: u32,
    best_bid_tick: Option<u64>,
    best_ask_tick: Option<u64>,
}

impl OrderBook {
    pub fn new(config: InstrumentConfig) -> Self {
        let n = config.max_ticks as usize;
        Self {
            instrument_id: config.id,
            bids: vec![PriceLevel::EMPTY; n],
            asks: vec![PriceLevel::EMPTY; n],
            base_price: config.base_price,
            max_ticks: config.max_ticks,
            best_bid_tick: None,
            best_ask_tick: None,
        }
    }

    /// Convert price (ticks) to array index.
    #[inline]
    fn price_to_index(&self, price: u64) -> usize {
        (price - self.base_price) as usize
    }

    /// Insert an order at the given price level. Returns the ArenaIndex.
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
        let idx = self.price_to_index(price);
        let levels = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let level = &mut levels[idx];

        // Append to tail of doubly-linked list
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

        // Update best price
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
    pub fn best_bid(&self) -> Option<u64> {
        self.best_bid_tick
    }

    #[inline]
    pub fn best_ask(&self) -> Option<u64> {
        self.best_ask_tick
    }

    /// Get bid level at given price. For testing/inspection.
    pub fn get_bid_level(&self, price: u64) -> &PriceLevel {
        &self.bids[self.price_to_index(price)]
    }

    /// Get ask level at given price. For testing/inspection.
    pub fn get_ask_level(&self, price: u64) -> &PriceLevel {
        &self.asks[self.price_to_index(price)]
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
git commit -m "feat(book): add tick-array order book with insert and best price tracking"
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

    let level_idx = self.price_to_index(price);
    let level = match side {
        Side::Bid => &mut self.bids[level_idx],
        Side::Ask => &mut self.asks[level_idx],
    };

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

    arena.free(idx);

    // If level is now empty and was the best price, scan for new best
    if level.is_empty() {
        match side {
            Side::Bid => {
                if self.best_bid_tick == Some(price) {
                    self.best_bid_tick = self.scan_best_bid(price);
                }
            }
            Side::Ask => {
                if self.best_ask_tick == Some(price) {
                    self.best_ask_tick = self.scan_best_ask(price);
                }
            }
        }
    }

    (side, price)
}

/// Scan downward from `from_price` (exclusive) to find next non-empty bid level.
fn scan_best_bid(&self, from_price: u64) -> Option<u64> {
    if from_price <= self.base_price {
        return None;
    }
    let start = self.price_to_index(from_price);
    for i in (0..start).rev() {
        if !self.bids[i].is_empty() {
            return Some(self.base_price + i as u64);
        }
    }
    None
}

/// Scan upward from `from_price` (exclusive) to find next non-empty ask level.
fn scan_best_ask(&self, from_price: u64) -> Option<u64> {
    let start = self.price_to_index(from_price);
    for i in (start + 1)..self.max_ticks as usize {
        if !self.asks[i].is_empty() {
            return Some(self.base_price + i as u64);
        }
    }
    None
}
```

**Step 4: Run tests**

Run: `cargo test -p matchx-book`
Expected: 8 tests PASS

**Step 5: Commit**

```bash
git add crates/matchx-book/
git commit -m "feat(book): add order removal with doubly-linked list unlinking and BBO scan"
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
```

**Step 2: Run to verify fail**

Run: `cargo test -p matchx-book`
Expected: FAIL — lookup not defined

**Step 3: Add HashMap-based order index**

Add `hashbrown` dependency to `crates/matchx-book/Cargo.toml` (no_std compatible HashMap):

```toml
[dependencies]
matchx-types.workspace = true
matchx-arena.workspace = true
hashbrown = "0.15"
```

Add to OrderBook struct:

```rust
use hashbrown::HashMap;

// In OrderBook struct:
order_index: HashMap<OrderId, ArenaIndex>,
```

In `new()`:
```rust
order_index: HashMap::new(),
```

In `insert_order()`, after arena alloc:
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
Expected: 10 tests PASS

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

/// Pluggable matching policy trait.
pub trait MatchPolicy {
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

pub struct MatchingEngine {
    book: OrderBook,
    arena: Arena,
    policy: PriceTimeFifo,
    sequence: u64,
}

impl MatchingEngine {
    pub fn new(config: InstrumentConfig, arena_capacity: u32) -> Self {
        Self {
            book: OrderBook::new(config),
            arena: Arena::new(arena_capacity),
            policy: PriceTimeFifo,
            sequence: 0,
        }
    }

    pub fn process(&mut self, cmd: Command) -> Vec<MatchEvent> {
        let mut events = Vec::new();
        match cmd {
            Command::NewOrder {
                id, side, price, qty, order_type, time_in_force,
                visible_qty, stop_price, stp_group, ..
            } => {
                self.process_new_order(
                    id, side, price, qty, order_type, time_in_force,
                    visible_qty, stop_price, stp_group, &mut events,
                );
            }
            Command::CancelOrder { id } => {
                self.process_cancel(id, &mut events);
            }
            Command::ModifyOrder { id, new_price, new_qty } => {
                self.process_modify(id, new_price, new_qty, &mut events);
            }
        }
        events
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
        events: &mut Vec<MatchEvent>,
    ) {
        events.push(MatchEvent::OrderAccepted {
            id,
            side,
            price,
            qty,
            order_type,
        });

        // Match against opposing side
        let fills = self.match_against_book(id, side, price, &mut qty);

        for fill in &fills {
            events.push(MatchEvent::Fill {
                maker_id: fill.maker_id,
                taker_id: fill.taker_id,
                price: fill.price,
                qty: fill.qty,
                maker_remaining: self.arena.get(fill.maker_idx).remaining(),
                taker_remaining: qty,
            });

            // Remove fully filled makers
            if self.arena.get(fill.maker_idx).is_filled() {
                self.book.remove_order(fill.maker_idx, &mut self.arena);
            }
        }

        // Rest remainder on book (for GTC limit orders)
        if qty > 0 && order_type == OrderType::Limit && time_in_force == TimeInForce::GTC {
            self.book.insert_order(id, side, price, qty, &mut self.arena);
        }
    }

    fn match_against_book(
        &mut self,
        taker_id: OrderId,
        taker_side: Side,
        taker_price: u64,
        remaining: &mut u64,
    ) -> Vec<Fill> {
        let mut fills = Vec::new();

        loop {
            if *remaining == 0 {
                break;
            }

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

                fills.push(Fill {
                    maker_idx,
                    maker_id,
                    taker_id,
                    price: resting_price,
                    qty: fill_qty,
                });
            }

            // After walking, check if best needs update (level may be drained)
            // This is handled by remove_order calls in the caller
            // But we need to break if we can't make progress
            let new_best = match taker_side {
                Side::Bid => self.book.best_ask(),
                Side::Ask => self.book.best_bid(),
            };
            if new_best == best_price && *remaining > 0 {
                // Same level still has unfilled orders we can't fill — break
                break;
            }
        }

        fills
    }

    fn process_cancel(&mut self, id: OrderId, events: &mut Vec<MatchEvent>) {
        if let Some(idx) = self.book.lookup(id) {
            let remaining = self.arena.get(idx).remaining();
            self.book.remove_order(idx, &mut self.arena);
            events.push(MatchEvent::OrderCancelled {
                id,
                remaining_qty: remaining,
            });
        } else {
            events.push(MatchEvent::OrderRejected {
                id,
                reason: RejectReason::OrderNotFound,
            });
        }
    }

    fn process_modify(
        &mut self,
        id: OrderId,
        new_price: u64,
        new_qty: u64,
        events: &mut Vec<MatchEvent>,
    ) {
        // Cancel + replace: remove old, insert new
        if let Some(idx) = self.book.lookup(id) {
            let order = self.arena.get(idx);
            let side = order.side;
            self.book.remove_order(idx, &mut self.arena);
            self.book.insert_order(id, side, new_price, new_qty, &mut self.arena);
            events.push(MatchEvent::OrderModified {
                id,
                new_price,
                new_qty,
            });
        } else {
            events.push(MatchEvent::OrderRejected {
                id,
                reason: RejectReason::OrderNotFound,
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
    &mut self.bids[self.price_to_index(price)]
}

pub fn get_ask_level_mut(&mut self, price: u64) -> &mut PriceLevel {
    &mut self.asks[self.price_to_index(price)]
}
```

**Step 5: Run tests**

Run: `cargo test -p matchx-engine`
Expected: 4 tests PASS

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
```

**Step 2: Run to verify fail**

Run: `cargo test -p matchx-engine`
Expected: new tests FAIL

**Step 3: Implement IOC/FOK/Market logic**

Modify `process_new_order` to handle time-in-force after matching:

```rust
// In process_new_order, after matching and fill events:

match (order_type, time_in_force) {
    // Market and IOC: cancel any unfilled remainder
    (OrderType::Market, _) | (_, TimeInForce::IOC) => {
        if qty > 0 {
            events.push(MatchEvent::OrderCancelled {
                id,
                remaining_qty: qty,
            });
        }
    }
    // GTC Limit: rest remainder on book
    (OrderType::Limit, TimeInForce::GTC) => {
        if qty > 0 {
            self.book.insert_order(id, side, price, qty, &mut self.arena);
        }
    }
    _ => {}
}
```

For FOK, add pre-check before matching:

```rust
// Before matching, if FOK, check available liquidity
if time_in_force == TimeInForce::FOK {
    let available = self.check_available_liquidity(side, price, qty);
    if available < qty {
        events.push(MatchEvent::OrderRejected {
            id,
            reason: RejectReason::InsufficientLiquidity,
        });
        return;
    }
}
```

Add helper:

```rust
fn check_available_liquidity(&self, taker_side: Side, taker_price: u64, needed: u64) -> u64 {
    let mut available = 0u64;
    // Walk opposing book levels checking availability
    // (read-only scan, no mutations)
    // Implementation walks price levels same as matching but only counts
    // ...
    available
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
Expected: 8 tests PASS

**Step 5: Commit**

```bash
git add crates/matchx-engine/
git commit -m "feat(engine): add Market, IOC, and FOK order support"
```

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
        events.push(MatchEvent::OrderRejected {
            id,
            reason: RejectReason::WouldCrossSpread,
        });
        return;
    }
    // Post-only goes directly to book, no matching
    events.push(MatchEvent::OrderAccepted { id, side, price, qty, order_type });
    self.book.insert_order(id, side, price, qty, &mut self.arena);
    return;
}
```

**Step 3: Run tests**

Run: `cargo test -p matchx-engine`
Expected: 10 tests PASS

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

**Step 2: Run to verify fail, then implement**

In `match_against_book`, before generating a fill, check STP:

```rust
// If both orders share stp_group, apply STP mode
if let (Some(taker_stp), Some(maker_stp)) = (taker_stp_group, maker.stp_group) {
    if taker_stp == maker_stp {
        // For CancelNewest: reject the incoming order entirely
        return (fills, true); // true = STP triggered
    }
}
```

Then in `process_new_order`, handle the STP signal by emitting rejection.

**Step 3: Run tests, commit**

Run: `cargo test -p matchx-engine`
Expected: 11 tests PASS

```bash
git add crates/matchx-engine/
git commit -m "feat(engine): add self-trade prevention (CancelNewest mode)"
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
fn stop_limit_triggers_when_price_crosses() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    // Stop-limit buy: trigger at 105, limit at 110
    engine.process(Command::NewOrder {
        id: OrderId(1), instrument_id: 1, side: Side::Bid, price: 110, qty: 10,
        order_type: OrderType::StopLimit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: Some(105), stp_group: None,
    });
    // Resting sell at 100
    engine.process(Command::NewOrder {
        id: OrderId(2), instrument_id: 1, side: Side::Ask, price: 100, qty: 5,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    // Trade at 100 pushes last price; then someone sells at 105
    // triggering the stop
    engine.process(Command::NewOrder {
        id: OrderId(3), instrument_id: 1, side: Side::Bid, price: 100, qty: 5,
        order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
        visible_qty: None, stop_price: None, stp_group: None,
    });
    // Now place a sell at 105 — this should trigger stop buy
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
    // Stop should have triggered
    assert!(events.iter().any(|e| matches!(e, MatchEvent::StopTriggered { .. })));
}
```

**Step 2: Implement stop order storage and trigger logic**

Add `stop_bids: BTreeMap<u64, VecDeque<ArenaIndex>>` and `stop_asks` to OrderBook. After each trade, check if the last trade price crosses any stop levels. Triggered stops become regular limit orders and enter matching.

**Step 3: Run tests, commit**

```bash
git add crates/matchx-engine/ crates/matchx-book/
git commit -m "feat(engine): add Stop-Limit order support with trigger on price cross"
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
            cmds.iter().map(|c| engine.process(c.clone())).collect()
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

### Task 14: Event Journal — Write and Read (matchx-journal)

**Files:**
- Create: `crates/matchx-journal/src/lib.rs`
- Create: `crates/matchx-journal/src/writer.rs`
- Create: `crates/matchx-journal/src/reader.rs`

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

        // Corrupt a byte
        let mut data = std::fs::read(&path).unwrap();
        data[10] ^= 0xFF;
        std::fs::write(&path, &data).unwrap();

        let mut reader = JournalReader::open(&path).unwrap();
        assert!(reader.read_all().is_err());
    }
}
```

**Step 2: Implement journal writer/reader**

Binary format: `[u32 length][u64 sequence][command bytes...][u32 crc32]`

Use `crc32fast` crate for CRC. Command serialization via a simple `to_bytes`/`from_bytes` on Command (hand-written binary layout, no serde).

Add to `crates/matchx-journal/Cargo.toml`:
```toml
[dependencies]
matchx-types.workspace = true
crc32fast = "1"

[dev-dependencies]
tempfile = "3"
```

**Step 3: Run tests, commit**

```bash
git add crates/matchx-journal/
git commit -m "feat(journal): add append-only binary journal with CRC32 integrity"
```

---

### Task 15: Event Journal — Deterministic Replay Integration Test

**Files:**
- Create: `tests/replay_determinism.rs`

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
        outputs1.push(engine1.process(cmd.clone()));
    }
    drop(writer);

    // Run 2: replay from journal
    let mut engine2 = MatchingEngine::new(config, 1024);
    let mut reader = JournalReader::open(&path).unwrap();
    let entries = reader.read_all().unwrap();
    let mut outputs2 = Vec::new();

    for entry in &entries {
        outputs2.push(engine2.process(entry.command.clone()));
    }

    assert_eq!(outputs1, outputs2, "Replay output diverged from original");
}
```

**Step 2: Run test, commit**

Run: `cargo test --test replay_determinism`
Expected: PASS

```bash
git add tests/
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
Expected: outputs ns/iter for each benchmark

**Step 3: Commit**

```bash
git add crates/matchx-bench/
git commit -m "bench: add criterion benchmarks for insert, trade, and cancel operations"
```

---

## Summary

| Task | Crate | What |
|------|-------|------|
| 1 | workspace | Scaffold 6-crate workspace |
| 2 | matchx-types | Core types: Order, PriceLevel, events, commands |
| 3 | matchx-arena | Pre-allocated arena with free-list |
| 4 | matchx-book | Tick-array order book: insert + BBO |
| 5 | matchx-book | Order removal + linked list unlinking |
| 6 | matchx-book | HashMap order index for cancel/modify |
| 7 | matchx-engine | MatchPolicy trait + PriceTimeFIFO + basic limit/cancel/modify |
| 8 | matchx-engine | Market + IOC + FOK orders |
| 9 | matchx-engine | Post-Only orders |
| 10 | matchx-engine | Self-trade prevention |
| 11 | matchx-engine | Iceberg orders |
| 12 | matchx-engine | Stop-Limit orders |
| 13 | matchx-engine | Property-based tests (proptest) |
| 14 | matchx-journal | Binary journal writer/reader with CRC32 |
| 15 | integration | End-to-end replay determinism test |
| 16 | matchx-bench | Criterion benchmarks |
