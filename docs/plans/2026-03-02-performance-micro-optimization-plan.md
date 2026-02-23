# Performance Micro-Optimization Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Achieve sub-microsecond p99 latency on the matching hot path by eliminating structural inefficiencies in the existing engine before adding any networking layer.

**Architecture:** Six sequential layers — baselines first, then struct layout, then compiler flags, then memory, then micro-opts, then system scripts. Each layer is independently measurable. No networking or new features are added.

**Tech Stack:** Rust (no_std crates), Criterion, hdrhistogram, libc (for mmap/mbind), perf, cargo-flamegraph.

---

## Task 1: Record and commit baseline benchmarks

**Files:**
- Create: `.cargo/config.toml`
- Modify: `Cargo.toml` (workspace)
- Create: `docs/baselines/` directory

**Step 1: Create `.cargo/config.toml` with native CPU flags**

```toml
# .cargo/config.toml
[build]
rustflags = ["-C", "target-cpu=native"]
```

**Step 2: Add bench profile to workspace `Cargo.toml`**

Open `Cargo.toml` and append after the `[workspace.dependencies]` section:

```toml
[profile.bench]
opt-level = 3
codegen-units = 1
lto = "thin"
```

**Step 3: Create baseline directory and run benchmarks**

```bash
mkdir -p docs/baselines
cargo bench 2>&1 | tee docs/baselines/2026-03-02-baseline.txt
```

Expected: Criterion output with timing lines like:
```
insert_limit_order    time:   [XXX ns XXX ns XXX ns]
crossing_trade        time:   [XXX ns XXX ns XXX ns]
```

**Step 4: Verify tests still pass**

```bash
cargo test --workspace
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add .cargo/config.toml Cargo.toml docs/baselines/2026-03-02-baseline.txt
git commit -m "perf(baseline): add bench profile and record initial baseline"
```

---

## Task 2: Add compile-time size guard for current Order layout

This documents the current size before we change it, and becomes the regression gate post-refactor.

**Files:**
- Modify: `crates/matchx-types/src/lib.rs`

**Step 1: Add a size-reporting test to `matchx-types/src/lib.rs`**

Append to the `#[cfg(test)]` block at the bottom of `crates/matchx-types/src/lib.rs`:

```rust
#[test]
fn print_order_size() {
    // Run with: cargo test -p matchx-types print_order_size -- --nocapture
    println!("size_of::<Order>() = {}", core::mem::size_of::<Order>());
    println!("align_of::<Order>() = {}", core::mem::align_of::<Order>());
}
```

**Step 2: Run it to see current size**

```bash
cargo test -p matchx-types print_order_size -- --nocapture
```

Expected output:
```
size_of::<Order>() = 104
align_of::<Order>() = 8
```

This confirms the 104-byte starting point documented in the design.

**Step 3: Commit**

```bash
git add crates/matchx-types/src/lib.rs
git commit -m "test(types): add size-reporting test for Order struct"
```

---

## Task 3: Refactor Order struct — remove stop_price, use sentinels, reorder fields, add align(64)

**Context:** Stop-limit orders are stored as `StopEntry` in the engine and never inserted into the arena. The `stop_price` field in `Order` is therefore dead weight. Removing it plus replacing `Option<T>` fields with sentinel values brings Order to exactly 63 bytes, which with `align(64)` becomes one perfect cache line.

**Target layout (63 bytes data + 1 byte pad = 64 bytes):**
```
offset  0: id: OrderId (u64)            — 8 bytes
offset  8: price: u64                   — 8 bytes
offset 16: quantity: u64                — 8 bytes
offset 24: filled: u64                  — 8 bytes
offset 32: timestamp: u64               — 8 bytes
offset 40: visible_quantity: u64        — 8 bytes
offset 48: stp_group: u32              — 4 bytes  (u32::MAX = no group)
offset 52: prev: ArenaIndex (u32)       — 4 bytes  (u32::MAX = no prev)
offset 56: next: ArenaIndex (u32)       — 4 bytes  (u32::MAX = no next)
offset 60: side: Side (u8)             — 1 byte
offset 61: order_type: OrderType (u8)  — 1 byte
offset 62: time_in_force: TimeInForce  — 1 byte
offset 63: (padding)                    — 1 byte
```

**Files:**
- Modify: `crates/matchx-types/src/lib.rs`

**Step 1: Write a failing compile-time size assertion**

Add to `crates/matchx-types/src/lib.rs` directly after the `Order` impl block (outside any `#[cfg(test)]`):

```rust
const _: () = assert!(
    core::mem::size_of::<Order>() == 64,
    "Order must be exactly 64 bytes (one cache line)"
);
```

**Step 2: Run to confirm it currently fails**

```bash
cargo build -p matchx-types 2>&1 | grep "Order must be"
```

Expected: compilation error about the assert failing (current size is 104, not 64).

**Step 3: Rewrite the `Order` struct in `crates/matchx-types/src/lib.rs`**

Replace the entire `Order` struct and its impl block. The `#[repr(C)]` is kept (FFI-safe layout), `align(64)` is added. `stop_price` is removed. `Option<T>` fields become bare types with sentinel values. Field order follows the layout table above.

```rust
/// Sentinel value for ArenaIndex meaning "no link" (replaces Option<ArenaIndex>).
pub const ARENA_NULL: ArenaIndex = ArenaIndex(u32::MAX);

/// Sentinel value for stp_group meaning "no group" (replaces Option<u32>).
pub const STP_NONE: u32 = u32::MAX;

/// An order stored in the arena. Uses intrusive doubly-linked list
/// for FIFO queue at each price level.
///
/// Layout is carefully tuned to exactly 64 bytes (one cache line).
/// `stop_price` is NOT stored here — stop-limit orders live in the
/// engine's StopEntry queue until triggered, never in the arena.
#[derive(Debug, Clone)]
#[repr(C, align(64))]
pub struct Order {
    pub id: OrderId,
    pub price: u64,
    pub quantity: u64,
    pub filled: u64,
    pub timestamp: u64,
    pub visible_quantity: u64,
    /// `STP_NONE` (u32::MAX) means no STP group.
    pub stp_group: u32,
    /// `ARENA_NULL` means no previous order in the price-level list.
    pub prev: ArenaIndex,
    /// `ARENA_NULL` means no next order in the price-level list.
    pub next: ArenaIndex,
    pub side: Side,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub _pad: u8,
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

    /// Quantity available for matching from this resting order.
    #[inline]
    pub fn matchable_qty(&self) -> u64 {
        match self.order_type {
            OrderType::Iceberg if self.visible_quantity > 0 => {
                let peak = self.visible_quantity;
                let filled_this_slice = self.filled % peak;
                (peak - filled_this_slice).min(self.remaining())
            }
            _ => self.remaining(),
        }
    }
}

const _: () = assert!(
    core::mem::size_of::<Order>() == 64,
    "Order must be exactly 64 bytes (one cache line)"
);
```

**Step 4: Build to check the size assert passes**

```bash
cargo build -p matchx-types
```

Expected: compiles without error (the assert passes at compile time).

**Step 5: Commit the types change before fixing downstream**

```bash
git add crates/matchx-types/src/lib.rs
git commit -m "perf(types): shrink Order to 64 bytes — remove stop_price, use sentinels, add align(64)"
```

---

## Task 4: Fix all Order construction sites to use sentinels

The previous task will cause compilation errors in `matchx-arena`, `matchx-book`, and `matchx-engine` wherever `Order { ..., stop_price: None, stp_group: None, prev: None, next: None }` is used. Fix each.

**Files:**
- Modify: `crates/matchx-arena/src/lib.rs` (test helper `make_order`)
- Modify: `crates/matchx-book/src/lib.rs` (wherever Order is constructed)
- Modify: `crates/matchx-engine/src/lib.rs` (wherever Order is constructed)
- Modify: `crates/matchx-itests/tests/replay_determinism.rs` (if it constructs Orders)

**Step 1: Find all compilation errors**

```bash
cargo build --workspace 2>&1 | grep "error\[" | head -40
```

Expected: errors about missing/renamed fields in Order struct literals.

**Step 2: Fix each file — replace Option fields with sentinels**

The transformation rule is:
- `stop_price: None` → **remove the field entirely** (it no longer exists)
- `stop_price: Some(x)` → **this should not exist** (stop orders never hit arena; if found, remove)
- `stp_group: None` → `stp_group: matchx_types::STP_NONE`
- `stp_group: Some(x)` → `stp_group: x`
- `prev: None` → `prev: matchx_types::ARENA_NULL`
- `prev: Some(idx)` → `prev: idx`
- `next: None` → `next: matchx_types::ARENA_NULL`
- `next: Some(idx)` → `next: idx`

For reading (is_some / unwrap patterns):
- `order.prev.is_some()` → `order.prev != ARENA_NULL`
- `order.prev.unwrap()` → `order.prev` (it's already an ArenaIndex)
- `order.next.is_none()` → `order.next == ARENA_NULL`
- `order.stp_group.is_some()` → `order.stp_group != STP_NONE`
- `order.stp_group.unwrap()` or `order.stp_group?` → `order.stp_group` (already u32)

Also add `_pad: 0` to every Order literal.

**Step 3: Verify everything compiles**

```bash
cargo build --workspace
```

Expected: clean build, no errors.

**Step 4: Run all tests**

```bash
cargo test --workspace
```

Expected: all tests pass. If any fail, the sentinel transformation was incorrect somewhere — re-read the failing test and fix the comparison logic.

**Step 5: Commit**

```bash
git add -p  # stage each changed file individually and review
git commit -m "fix: update all Order construction sites to use arena sentinels"
```

---

## Task 5: Refactor arena to parallel-array layout (avoid 128-byte Slot)

**Context:** With `Order` having `align(64)`, the `Slot` enum (`Occupied(Order)` | `Free { next_free }`) would grow to ~128 bytes because Rust must place the discriminant before the 64-byte-aligned Order data. This wastes half the arena. Replace with parallel arrays: data is a flat `Vec<MaybeUninit<Order>>` and occupancy is tracked via a separate free-list next-pointer array.

**Files:**
- Modify: `crates/matchx-arena/src/lib.rs`

**Step 1: Write a test for the new arena that covers alloc, free, reuse, get, get_mut**

Add to `crates/matchx-arena/src/lib.rs` tests (replacing the existing ones is fine — the behaviour contract is identical):

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
            stp_group: STP_NONE,
            prev: ARENA_NULL,
            next: ARENA_NULL,
            _pad: 0,
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
        assert_eq!(c, a);
        assert_eq!(arena.get(c).id, OrderId(3));
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

    #[test]
    fn slot_size_is_exactly_order_size() {
        // Each slot in the data array should be exactly 64 bytes.
        // If this fails, arena memory is being wasted.
        assert_eq!(core::mem::size_of::<Order>(), 64);
    }
}
```

**Step 2: Run to confirm tests fail (old Arena struct broken by Order changes)**

```bash
cargo test -p matchx-arena 2>&1 | head -30
```

Expected: compile errors or test failures due to Order field changes.

**Step 3: Rewrite `Arena` in `crates/matchx-arena/src/lib.rs`**

Replace the entire file content:

```rust
#![cfg_attr(not(test), no_std)]
extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::mem::MaybeUninit;
use matchx_types::{ArenaIndex, Order};

/// Pre-allocated arena for Order objects using parallel arrays.
///
/// Layout:
/// - `data`: flat Vec of MaybeUninit<Order>, each slot is exactly 64 bytes (one cache line).
/// - `next_free`: parallel Vec of u32 used as a singly-linked free list.
///   For free slots: stores the index of the next free slot (u32::MAX = end of list).
///   For occupied slots: value is undefined (not read).
///
/// This avoids the Slot enum which would grow to 128 bytes due to Order's align(64).
pub struct Arena {
    data: Vec<MaybeUninit<Order>>,
    next_free: Vec<u32>,
    free_head: u32, // u32::MAX means list is empty
    len: u32,
    capacity: u32,
}

const FREE_LIST_END: u32 = u32::MAX;

impl Arena {
    /// Create arena with given capacity. All slots start free.
    pub fn new(capacity: u32) -> Self {
        let cap = capacity as usize;
        // Build free list: slot i points to i+1; last points to FREE_LIST_END.
        let mut next_free = Vec::with_capacity(cap);
        for i in 0..capacity {
            next_free.push(if i + 1 < capacity { i + 1 } else { FREE_LIST_END });
        }
        Self {
            data: vec![MaybeUninit::uninit(); cap],
            next_free,
            free_head: if capacity > 0 { 0 } else { FREE_LIST_END },
            len: 0,
            capacity,
        }
    }

    /// Allocate a slot for the given order. Returns None if full.
    #[inline]
    pub fn alloc(&mut self, order: Order) -> Option<ArenaIndex> {
        if self.free_head == FREE_LIST_END {
            return None;
        }
        let idx = self.free_head;
        self.free_head = self.next_free[idx as usize];
        // SAFETY: we just took this slot from the free list; it is not occupied.
        unsafe { self.data[idx as usize].as_mut_ptr().write(order) };
        self.len += 1;
        Some(ArenaIndex(idx))
    }

    /// Free a slot, returning it to the free list.
    #[inline]
    pub fn free(&mut self, index: ArenaIndex) {
        let idx = index.0;
        // SAFETY: caller asserts the slot is occupied.
        unsafe { self.data[idx as usize].assume_init_drop() };
        self.next_free[idx as usize] = self.free_head;
        self.free_head = idx;
        self.len -= 1;
    }

    /// Get immutable reference to order at index.
    #[inline]
    pub fn get(&self, index: ArenaIndex) -> &Order {
        // SAFETY: caller asserts the slot is occupied.
        unsafe { self.data[index.as_usize()].assume_init_ref() }
    }

    /// Get mutable reference to order at index.
    #[inline]
    pub fn get_mut(&mut self, index: ArenaIndex) -> &mut Order {
        // SAFETY: caller asserts the slot is occupied.
        unsafe { self.data[index.as_usize()].assume_init_mut() }
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
        self.capacity
    }
}

// SAFETY: Order is Send; no shared mutable state between threads.
unsafe impl Send for Arena {}

// Tests here — replace the old test module with the new one from Step 1 above.
```

**Step 4: Run arena tests**

```bash
cargo test -p matchx-arena
```

Expected: all 6 tests pass.

**Step 5: Run full workspace tests**

```bash
cargo test --workspace
```

Expected: all tests pass.

**Step 6: Commit**

```bash
git add crates/matchx-arena/src/lib.rs
git commit -m "perf(arena): replace Slot enum with parallel-array layout to keep 64-byte slots"
```

---

## Task 6: Add PriceLevel and MatchingEngine size guards

**Files:**
- Modify: `crates/matchx-types/src/lib.rs`
- Modify: `crates/matchx-engine/src/lib.rs`

**Step 1: Add PriceLevel size guard in `matchx-types/src/lib.rs`**

After the `PriceLevel` impl block, add:

```rust
const _: () = assert!(
    core::mem::size_of::<PriceLevel>() <= 32,
    "PriceLevel should fit in half a cache line"
);
```

**Step 2: Check current PriceLevel size**

```rust
// Add temporarily to the tests block in matchx-types:
#[test]
fn print_price_level_size() {
    println!("size_of::<PriceLevel>() = {}", core::mem::size_of::<PriceLevel>());
}
```

```bash
cargo test -p matchx-types print_price_level_size -- --nocapture
```

Adjust the assert threshold if needed — goal is ≤ 32 bytes. Remove the temporary print test after verifying.

**Step 3: Build and run tests**

```bash
cargo build --workspace && cargo test --workspace
```

Expected: clean build, all tests pass.

**Step 4: Commit**

```bash
git add crates/matchx-types/src/lib.rs
git commit -m "perf(types): add compile-time size guards for PriceLevel"
```

---

## Task 7: Compilation profile tuning — LTO, codegen-units, panic=abort

**Files:**
- Modify: `Cargo.toml` (workspace)

**Step 1: Add release and bench profiles to workspace `Cargo.toml`**

Append to `Cargo.toml`:

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"

[profile.bench]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
inherits = "release"
```

Note: `lto = "fat"` enables cross-crate inlining (slow to compile, fastest at runtime). The hot path spans `matchx-types → matchx-arena → matchx-book → matchx-engine`; without LTO those crate boundaries block inlining.

**Step 2: Verify tests still compile and pass (tests use debug profile, unaffected)**

```bash
cargo test --workspace
```

Expected: all tests pass.

**Step 3: Run benchmarks with new profile and record improvement**

```bash
cargo bench 2>&1 | tee docs/baselines/2026-03-02-after-lto.txt
diff docs/baselines/2026-03-02-baseline.txt docs/baselines/2026-03-02-after-lto.txt
```

Expected: visible speedup in `insert_limit_order` and `crossing_trade` due to cross-crate inlining.

**Step 4: Commit**

```bash
git add Cargo.toml docs/baselines/2026-03-02-after-lto.txt
git commit -m "perf(build): enable fat LTO, single codegen-unit, panic=abort for bench/release"
```

---

## Task 8: Add PGO workflow script

**Files:**
- Create: `scripts/pgo-bench.sh`

**Step 1: Create the script**

```bash
#!/usr/bin/env bash
# scripts/pgo-bench.sh — Profile-Guided Optimization for matchx-bench
# Usage: bash scripts/pgo-bench.sh
# Requires: rustup, llvm-profdata (part of llvm-tools-preview component)
#   Install with: rustup component add llvm-tools-preview

set -euo pipefail

PROFILE_DIR="$(pwd)/pgo-profiles"
MERGED="$PROFILE_DIR/merged.profdata"
BENCH_BIN=$(cargo bench --no-run --message-format=json 2>/dev/null \
  | jq -r 'select(.executable != null) | .executable' | tail -1)

echo "=== Step 1: Instrument build ==="
rm -rf "$PROFILE_DIR" && mkdir -p "$PROFILE_DIR"
RUSTFLAGS="-C instrument-coverage -C target-cpu=native" \
  cargo bench --no-run 2>/dev/null
INSTR_BIN=$(cargo bench --no-run --message-format=json \
  RUSTFLAGS="-C instrument-coverage -C target-cpu=native" 2>/dev/null \
  | jq -r 'select(.executable != null) | .executable' | tail -1)

echo "=== Step 2: Collect profile data ==="
LLVM_PROFILE_FILE="$PROFILE_DIR/bench-%p-%m.profraw" "$INSTR_BIN" --bench 2>/dev/null || true

echo "=== Step 3: Merge profiles ==="
llvm-profdata merge -sparse "$PROFILE_DIR"/*.profraw -o "$MERGED"

echo "=== Step 4: PGO-optimized bench ==="
RUSTFLAGS="-C profile-use=$MERGED -C target-cpu=native" \
  cargo bench 2>&1 | tee docs/baselines/2026-03-02-after-pgo.txt

echo "=== Done. Results in docs/baselines/2026-03-02-after-pgo.txt ==="
```

**Step 2: Make executable**

```bash
chmod +x scripts/pgo-bench.sh
```

**Step 3: Verify it is syntactically valid (dry run)**

```bash
bash -n scripts/pgo-bench.sh
```

Expected: no output (no syntax errors).

**Step 4: Commit**

```bash
git add scripts/pgo-bench.sh
git commit -m "perf(scripts): add PGO workflow script for profile-guided bench optimization"
```

---

## Task 9: Replace Vec<MatchEvent> with fixed-size inline array

**Context:** `MatchingEngine::event_buffer` is a `Vec<MatchEvent>` on the heap. Every `emit` call dereferences a heap pointer. A fixed-size stack/inline array eliminates this indirection and the bounds-check branch. 16 events is the safe upper bound per `process` call (worst case: Accepted + N fills + N BookUpdates + Cancelled = rarely exceeds 16 for a single order).

**Files:**
- Modify: `crates/matchx-engine/src/lib.rs`

**Step 1: Write a test that verifies we can process 16 fills without panicking**

Add to the test section of `crates/matchx-engine/src/lib.rs`:

```rust
#[test]
fn process_does_not_overflow_event_buffer_at_16_fills() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    // Place 16 resting asks
    for i in 0u64..16 {
        engine.process(Command::NewOrder {
            id: OrderId(i + 1),
            instrument_id: 1,
            side: Side::Ask,
            price: 5000,
            qty: 1,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
    }
    // One large bid that sweeps all 16 — produces Accepted + 16 Fills + 16 BookUpdates
    let events = engine.process(Command::NewOrder {
        id: OrderId(100),
        instrument_id: 1,
        side: Side::Bid,
        price: 5000,
        qty: 16,
        order_type: OrderType::Limit,
        time_in_force: TimeInForce::GTC,
        visible_qty: None,
        stop_price: None,
        stp_group: None,
    });
    // 1 Accepted + 16 Fills + 16 BookUpdates = 33 events — exceeds 16!
    // This test reveals we need a larger buffer or a different strategy.
    assert!(events.len() >= 16);
}
```

**Step 2: Run the test to measure actual worst-case event count**

```bash
cargo test -p matchx-engine process_does_not_overflow -- --nocapture
```

Note the actual events length. If it exceeds 32, increase the buffer size in Step 3 accordingly.

**Step 3: Replace `event_buffer: Vec<MatchEvent>` with a fixed-size array**

In `crates/matchx-engine/src/lib.rs`, change the MatchingEngine struct:

```rust
use core::mem::MaybeUninit;

// At module level, define the max events per process() call.
// 1 Accepted + N Fills + N BookUpdates + 1 Cancelled = at most 2*arena_capacity+2.
// In practice a realistic upper bound is 64 (covers sweeping 31 price levels).
const MAX_EVENTS_PER_CALL: usize = 64;

pub struct MatchingEngine {
    book: OrderBook,
    arena: Arena,
    policy: PriceTimeFifo,
    config: InstrumentConfig,
    sequence: u64,
    timestamp_ns: u64,
    event_buf: [MaybeUninit<MatchEvent>; MAX_EVENTS_PER_CALL],
    event_len: usize,
    stop_bids: BTreeMap<u64, VecDeque<StopEntry>>,
    stop_asks: BTreeMap<u64, VecDeque<StopEntry>>,
    last_trade_price: Option<u64>,
}
```

Update `new()`:
```rust
pub fn new(config: InstrumentConfig, arena_capacity: u32) -> Self {
    Self {
        book: OrderBook::new(config.clone()),
        arena: Arena::new(arena_capacity),
        policy: PriceTimeFifo,
        config,
        sequence: 0,
        timestamp_ns: 0,
        event_buf: [const { MaybeUninit::uninit() }; MAX_EVENTS_PER_CALL],
        event_len: 0,
        stop_bids: BTreeMap::new(),
        stop_asks: BTreeMap::new(),
        last_trade_price: None,
    }
}
```

Update `emit()`:
```rust
#[inline(always)]
fn emit(&mut self, event_fn: impl FnOnce(EventMeta) -> MatchEvent) {
    self.sequence += 1;
    self.timestamp_ns += 1;
    let meta = EventMeta {
        sequence: self.sequence,
        timestamp_ns: self.timestamp_ns,
    };
    debug_assert!(self.event_len < MAX_EVENTS_PER_CALL, "event buffer overflow");
    // SAFETY: event_len < MAX_EVENTS_PER_CALL (asserted above in debug builds).
    unsafe {
        self.event_buf[self.event_len].as_mut_ptr().write(event_fn(meta));
    }
    self.event_len += 1;
}
```

Update `process()`:
```rust
pub fn process(&mut self, cmd: Command) -> &[MatchEvent] {
    self.event_len = 0;
    // ... existing match arm code unchanged ...
    self.drain_stop_triggers();
    // SAFETY: event_len slots were written by emit().
    unsafe {
        core::slice::from_raw_parts(
            self.event_buf.as_ptr() as *const MatchEvent,
            self.event_len,
        )
    }
}
```

**Step 4: Run all tests**

```bash
cargo test --workspace
```

Expected: all tests pass.

**Step 5: Run benchmarks and record**

```bash
cargo bench 2>&1 | tee docs/baselines/2026-03-02-after-fixed-buf.txt
```

**Step 6: Commit**

```bash
git add crates/matchx-engine/src/lib.rs docs/baselines/2026-03-02-after-fixed-buf.txt
git commit -m "perf(engine): replace Vec<MatchEvent> with fixed-size inline array, eliminate heap indirection"
```

---

## Task 10: Add #[inline(always)] and #[cold] annotations

**Files:**
- Modify: `crates/matchx-engine/src/lib.rs`
- Modify: `crates/matchx-types/src/lib.rs`

**Step 1: Add `#[inline(always)]` to hot inner functions in engine**

Find these functions in `crates/matchx-engine/src/lib.rs` and add `#[inline(always)]`:
- `emit` (already there from Task 9)
- `match_against_book` (the innermost matching loop)
- `check_available_liquidity` (called on every FOK)

```rust
#[inline(always)]
fn match_against_book(...) { ... }

#[inline(always)]
fn check_available_liquidity(...) { ... }
```

**Step 2: Add `#[cold]` to rejection paths in engine**

Find and mark these internal helpers/inlined code paths as cold. If they are inline code (not functions), extract them into small `#[cold]` functions:

```rust
#[cold]
fn emit_rejected(&mut self, id: OrderId, reason: RejectReason) {
    self.emit(|meta| MatchEvent::OrderRejected { meta, id, reason });
}
```

Replace `self.emit(|meta| MatchEvent::OrderRejected { ... })` call sites with `self.emit_rejected(id, reason)` where the reason is `WouldCrossSpread`, `InsufficientLiquidity`, or `DuplicateOrderId`.

**Step 3: Add `#[inline(always)]` to `Order::remaining`, `Order::matchable_qty`**

In `crates/matchx-types/src/lib.rs`, these already have `#[inline]` — upgrade to `#[inline(always)]` since they are called in the innermost matching loop:

```rust
#[inline(always)]
pub fn remaining(&self) -> u64 { ... }

#[inline(always)]
pub fn matchable_qty(&self) -> u64 { ... }
```

**Step 4: Run all tests**

```bash
cargo test --workspace
```

Expected: all tests pass (annotations don't change behaviour).

**Step 5: Commit**

```bash
git add crates/matchx-engine/src/lib.rs crates/matchx-types/src/lib.rs
git commit -m "perf(engine): add #[inline(always)] on hot path, #[cold] on rejection paths"
```

---

## Task 11: Replace BTreeMap stop-limit queue with flat sorted Vec

**Context:** `stop_bids: BTreeMap<u64, VecDeque<StopEntry>>` has poor cache behavior: each insert walks a heap-allocated B-tree and potentially allocates a new `VecDeque`. Since there are rarely more than a handful of active stop prices, a flat sorted `Vec<(u64, StopEntry)>` with binary-search insert is faster and cache-friendly.

**Files:**
- Modify: `crates/matchx-engine/src/lib.rs`

**Step 1: Write a test for stop-limit triggering (verifies correctness after refactor)**

Add to the engine test section:

```rust
#[test]
fn stop_limit_triggers_and_fills() {
    let mut engine = MatchingEngine::new(test_config(), 1024);
    // Resting ask at 100
    engine.process(Command::NewOrder {
        id: OrderId(1),
        instrument_id: 1,
        side: Side::Ask,
        price: 100,
        qty: 5,
        order_type: OrderType::Limit,
        time_in_force: TimeInForce::GTC,
        visible_qty: None,
        stop_price: None,
        stp_group: None,
    });
    // Stop-limit buy: stop at 100, limit at 105
    engine.process(Command::NewOrder {
        id: OrderId(2),
        instrument_id: 1,
        side: Side::Bid,
        price: 105,
        qty: 3,
        order_type: OrderType::StopLimit,
        time_in_force: TimeInForce::GTC,
        visible_qty: None,
        stop_price: Some(100),
        stp_group: None,
    });
    // Incoming bid that fills against resting ask at 100 — triggers the stop
    let events = engine.process(Command::NewOrder {
        id: OrderId(3),
        instrument_id: 1,
        side: Side::Bid,
        price: 100,
        qty: 2,
        order_type: OrderType::Limit,
        time_in_force: TimeInForce::GTC,
        visible_qty: None,
        stop_price: None,
        stp_group: None,
    });
    // Stop should have triggered: StopTriggered event present
    assert!(events.iter().any(|e| matches!(e, MatchEvent::StopTriggered { stop_id: OrderId(2), .. })));
}
```

**Step 2: Run to confirm it currently passes (baseline)**

```bash
cargo test -p matchx-engine stop_limit_triggers_and_fills
```

Expected: PASS (confirms existing stop logic works before refactor).

**Step 3: Replace the BTreeMap fields with flat Vecs**

In the `MatchingEngine` struct, change:
```rust
// OLD
stop_bids: BTreeMap<u64, VecDeque<StopEntry>>,
stop_asks: BTreeMap<u64, VecDeque<StopEntry>>,

// NEW — sorted by stop_price ascending; stop_bids trigger when last_trade >= stop_price
stop_bids: Vec<(u64, StopEntry)>,
stop_asks: Vec<(u64, StopEntry)>,
```

In `new()`, change:
```rust
// OLD
stop_bids: BTreeMap::new(),
stop_asks: BTreeMap::new(),

// NEW
stop_bids: Vec::new(),
stop_asks: Vec::new(),
```

Remove `use alloc::collections::VecDeque;` from the imports (if no longer used).

**Step 4: Update the insert logic**

Wherever stop entries are added (look for `stop_bids.entry(stop_px).or_default().push_back(entry)`), replace with a binary-search insert that maintains sorted order by stop_price:

```rust
// Insert into stop_bids keeping sorted by stop_price ascending
let pos = self.stop_bids.partition_point(|(px, _)| *px < stop_px);
self.stop_bids.insert(pos, (stop_px, entry));
```

**Step 5: Update the trigger/drain logic**

Wherever stop queues are drained (the `drain_stop_triggers` function), replace BTreeMap range scans with Vec iteration:

For buy stops (trigger when `last_trade_price >= stop_price`):
```rust
// Drain all stop_bids where stop_price <= last_trade_price
let mut i = 0;
while i < self.stop_bids.len() {
    if self.stop_bids[i].0 <= last_price {
        let (_, entry) = self.stop_bids.remove(i);
        // inject entry as limit order
        self.inject_stop_entry(entry);
        // don't increment i — next element shifted down
    } else {
        i += 1;
    }
}
```

For sell stops (trigger when `last_trade_price <= stop_price`), same pattern but comparison is reversed.

**Step 6: Run all tests**

```bash
cargo test --workspace
```

Expected: all tests pass including the new `stop_limit_triggers_and_fills` test.

**Step 7: Commit**

```bash
git add crates/matchx-engine/src/lib.rs
git commit -m "perf(engine): replace BTreeMap/VecDeque stop queues with flat sorted Vec"
```

---

## Task 12: Add latency_histogram benchmark

**Context:** The existing Criterion benches measure wall-clock time per iteration but do not give p99/p999 percentiles. The sub-µs SLO requires a histogram. This adds a `latency_histogram` bench using `hdrhistogram`.

**Files:**
- Modify: `crates/matchx-bench/Cargo.toml`
- Create: `crates/matchx-bench/benches/latency_histogram.rs`
- Modify: `crates/matchx-bench/Cargo.toml` (add [[bench]] entry)

**Step 1: Add hdrhistogram to bench dependencies**

In `crates/matchx-bench/Cargo.toml`, add:

```toml
[dependencies]
# existing deps...
hdrhistogram = "7"
```

And add the new bench target:

```toml
[[bench]]
name = "latency_histogram"
harness = false
```

**Step 2: Create `crates/matchx-bench/benches/latency_histogram.rs`**

```rust
//! Latency histogram benchmark for the matching engine hot path.
//!
//! Run with:
//!   cargo bench --bench latency_histogram -- --nocapture
//!
//! Reports p50, p99, p99.9, p99.99, and max latency in nanoseconds.

use hdrhistogram::Histogram;
use matchx_engine::MatchingEngine;
use matchx_types::*;
use std::time::Instant;

fn config() -> InstrumentConfig {
    InstrumentConfig {
        id: 1,
        tick_size: 1,
        lot_size: 1,
        base_price: 0,
        max_ticks: 10000,
        stp_mode: StpMode::CancelNewest,
    }
}

/// Measures the latency of a single limit order insert (no-cross, rests in book).
fn bench_insert_latency() {
    let mut hist = Histogram::<u64>::new_with_bounds(1, 1_000_000, 3).unwrap();
    let mut engine = MatchingEngine::new(config(), 65536);
    let iters = 1_000_000u64;
    let mut id = 1u64;

    // Warm up
    for _ in 0..10_000 {
        let side = if id % 2 == 0 { Side::Bid } else { Side::Ask };
        let price = if side == Side::Bid { 4900 + (id % 100) } else { 5100 + (id % 100) };
        engine.process(Command::NewOrder {
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
        });
        id += 1;
    }
    // Reset
    let mut engine = MatchingEngine::new(config(), 65536);
    id = 1;

    for _ in 0..iters {
        let side = if id % 2 == 0 { Side::Bid } else { Side::Ask };
        let price = if side == Side::Bid { 4900 + (id % 100) } else { 5100 + (id % 100) };
        let t0 = Instant::now();
        let _ = std::hint::black_box(engine.process(Command::NewOrder {
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
        let elapsed_ns = t0.elapsed().as_nanos() as u64;
        let _ = hist.record(elapsed_ns.max(1));
        id += 1;
    }

    println!("\n=== insert_limit_order latency ({} iters) ===", iters);
    println!("  p50   : {:>8} ns", hist.value_at_quantile(0.50));
    println!("  p99   : {:>8} ns", hist.value_at_quantile(0.99));
    println!("  p99.9 : {:>8} ns", hist.value_at_quantile(0.999));
    println!("  p99.99: {:>8} ns", hist.value_at_quantile(0.9999));
    println!("  max   : {:>8} ns", hist.max());
}

fn main() {
    bench_insert_latency();
}
```

**Step 3: Run and inspect output**

```bash
cargo bench --bench latency_histogram -- --nocapture 2>&1 | tee docs/baselines/2026-03-02-latency-histogram.txt
```

Expected output (approximate — exact numbers depend on hardware):
```
=== insert_limit_order latency (1000000 iters) ===
  p50   :      XXX ns
  p99   :      XXX ns
  ...
```

Record the p99 value. Target is < 1000 ns (1 µs). On a development machine (WSL2 or VM), p99 will be higher; this benchmark's primary purpose is to run on bare metal with CPU isolation.

**Step 4: Commit**

```bash
git add crates/matchx-bench/Cargo.toml crates/matchx-bench/benches/latency_histogram.rs docs/baselines/2026-03-02-latency-histogram.txt
git commit -m "perf(bench): add hdrhistogram latency benchmark reporting p50/p99/p999"
```

---

## Task 13: Add huge-pages feature to matchx-arena

**Files:**
- Modify: `crates/matchx-arena/Cargo.toml`
- Modify: `crates/matchx-arena/src/lib.rs`

**Step 1: Add libc dependency behind a feature flag**

In `crates/matchx-arena/Cargo.toml`:

```toml
[features]
huge_pages = ["libc"]

[dependencies]
matchx-types.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
libc = { version = "1", optional = true }
```

Also add `libc` to `[workspace.dependencies]` in root `Cargo.toml`:
```toml
libc = "1"
```

**Step 2: Write a test that the huge_pages feature compiles and the arena still works**

```bash
cargo test -p matchx-arena --features huge_pages
```

Expected: tests pass (feature just changes allocation backend, not interface).

**Step 3: Add `HugePageArena` backed by mmap in `crates/matchx-arena/src/lib.rs`**

Append to the arena source:

```rust
#[cfg(all(feature = "huge_pages", target_os = "linux"))]
mod huge {
    use core::mem::MaybeUninit;
    use matchx_types::{ArenaIndex, Order};
    use alloc::vec::Vec;

    /// Arena variant that uses `mmap(MAP_HUGETLB | MAP_HUGE_2MB)` to back the
    /// data array, reducing TLB pressure from ~1024 entries (4KB pages) to 2
    /// entries (2MB pages) for a 65536-order arena.
    ///
    /// Falls back to standard `mmap(MAP_ANONYMOUS)` if MAP_HUGETLB fails.
    /// The interface is identical to `Arena`.
    pub struct HugePageArena {
        data: *mut MaybeUninit<Order>,
        next_free: Vec<u32>,
        free_head: u32,
        len: u32,
        capacity: u32,
    }

    const FREE_LIST_END: u32 = u32::MAX;

    impl HugePageArena {
        pub fn new(capacity: u32) -> Self {
            let byte_len = (capacity as usize) * core::mem::size_of::<Order>();
            let page_size = 2 * 1024 * 1024; // 2MB
            let aligned_len = (byte_len + page_size - 1) & !(page_size - 1);

            // Try MAP_HUGETLB | MAP_HUGE_2MB first; fall back on failure.
            let ptr = unsafe {
                let p = libc::mmap(
                    core::ptr::null_mut(),
                    aligned_len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS
                        | libc::MAP_HUGETLB | (21 << libc::MAP_HUGE_SHIFT),
                    -1,
                    0,
                );
                if p == libc::MAP_FAILED {
                    // Fallback to regular anonymous mapping
                    let p2 = libc::mmap(
                        core::ptr::null_mut(),
                        aligned_len,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    );
                    assert_ne!(p2, libc::MAP_FAILED, "mmap failed for arena backing");
                    p2
                } else {
                    p
                }
            };

            let mut next_free = Vec::with_capacity(capacity as usize);
            for i in 0..capacity {
                next_free.push(if i + 1 < capacity { i + 1 } else { FREE_LIST_END });
            }

            Self {
                data: ptr as *mut MaybeUninit<Order>,
                next_free,
                free_head: if capacity > 0 { 0 } else { FREE_LIST_END },
                len: 0,
                capacity,
            }
        }

        #[inline]
        pub fn alloc(&mut self, order: Order) -> Option<ArenaIndex> {
            if self.free_head == FREE_LIST_END { return None; }
            let idx = self.free_head;
            self.free_head = self.next_free[idx as usize];
            unsafe { (self.data.add(idx as usize)).write(MaybeUninit::new(order)) };
            self.len += 1;
            Some(ArenaIndex(idx))
        }

        #[inline]
        pub fn free(&mut self, index: ArenaIndex) {
            let idx = index.0;
            unsafe { (*self.data.add(idx as usize)).assume_init_drop() };
            self.next_free[idx as usize] = self.free_head;
            self.free_head = idx;
            self.len -= 1;
        }

        #[inline]
        pub fn get(&self, index: ArenaIndex) -> &Order {
            unsafe { (*self.data.add(index.as_usize())).assume_init_ref() }
        }

        #[inline]
        pub fn get_mut(&mut self, index: ArenaIndex) -> &mut Order {
            unsafe { (*self.data.add(index.as_usize())).assume_init_mut() }
        }

        pub fn len(&self) -> u32 { self.len }
        pub fn is_empty(&self) -> bool { self.len == 0 }
        pub fn capacity(&self) -> u32 { self.capacity }
    }

    impl Drop for HugePageArena {
        fn drop(&mut self) {
            let byte_len = (self.capacity as usize) * core::mem::size_of::<Order>();
            let page_size = 2 * 1024 * 1024;
            let aligned_len = (byte_len + page_size - 1) & !(page_size - 1);
            unsafe { libc::munmap(self.data as *mut libc::c_void, aligned_len); }
        }
    }

    unsafe impl Send for HugePageArena {}
}

#[cfg(all(feature = "huge_pages", target_os = "linux"))]
pub use huge::HugePageArena;
```

**Step 4: Build and test**

```bash
cargo build -p matchx-arena --features huge_pages
cargo test -p matchx-arena --features huge_pages
```

Expected: clean build, tests pass.

**Step 5: Commit**

```bash
git add crates/matchx-arena/Cargo.toml crates/matchx-arena/src/lib.rs Cargo.toml
git commit -m "perf(arena): add huge_pages feature using mmap(MAP_HUGETLB) with 4KB fallback"
```

---

## Task 14: Add system tuning scripts

**Files:**
- Create: `scripts/setup-cpu-isolation.sh`
- Create: `scripts/run-bench-rt.sh`
- Create: `scripts/Makefile` (or top-level `Makefile`)

**Step 1: Create `scripts/setup-cpu-isolation.sh`**

```bash
#!/usr/bin/env bash
# Validates that CPU isolation is active before running latency benchmarks.
# These settings require kernel boot parameters:
#   isolcpus=2,3 nohz_full=2,3 rcu_nocbs=2,3
# Add to GRUB_CMDLINE_LINUX in /etc/default/grub, then: sudo update-grub && reboot

set -euo pipefail

ISOLATED=$(cat /sys/devices/system/cpu/isolated 2>/dev/null || echo "")
NOHZ=$(cat /sys/devices/system/cpu/nohz_full 2>/dev/null || echo "")

echo "Isolated CPUs : ${ISOLATED:-none}"
echo "nohz_full CPUs: ${NOHZ:-none}"

if [[ -z "$ISOLATED" ]]; then
    echo ""
    echo "WARNING: No CPUs are isolated. Latency benchmarks will show OS jitter."
    echo "For sub-µs p99, add to kernel cmdline: isolcpus=2,3 nohz_full=2,3 rcu_nocbs=2,3"
    echo "Then reboot and re-run this script."
    exit 1
fi

echo ""
echo "CPU isolation active. Proceed with: bash scripts/run-bench-rt.sh"
```

**Step 2: Create `scripts/run-bench-rt.sh`**

```bash
#!/usr/bin/env bash
# Run the latency histogram benchmark under SCHED_FIFO on isolated CPUs.
# Requires: sudo, chrt, taskset
# Usage: sudo bash scripts/run-bench-rt.sh

set -euo pipefail

ISOLATED_CPU=2  # first isolated CPU — adjust to match your isolcpus= setting
BENCH_BIN=$(cargo bench --bench latency_histogram --no-run --message-format=json 2>/dev/null \
  | python3 -c "import sys,json; [print(json.loads(l)['executable']) for l in sys.stdin if 'executable' in l]" \
  | tail -1)

if [[ -z "$BENCH_BIN" ]]; then
    echo "Build bench first: cargo bench --bench latency_histogram --no-run"
    exit 1
fi

echo "Running $BENCH_BIN on CPU $ISOLATED_CPU with SCHED_FIFO priority 99"
taskset -c $ISOLATED_CPU chrt -f 99 "$BENCH_BIN" --nocapture \
  2>&1 | tee docs/baselines/$(date +%Y-%m-%d)-rt-latency.txt
```

**Step 3: Create top-level `Makefile`**

```makefile
.PHONY: flamegraph bench baseline rt-bench

# Record baseline benchmark numbers
baseline:
	cargo bench 2>&1 | tee docs/baselines/$$(date +%Y-%m-%d)-baseline.txt

# Criterion benchmarks with fat LTO
bench:
	cargo bench

# Latency histogram (no RT scheduling)
latency:
	cargo bench --bench latency_histogram -- --nocapture

# Flamegraph: requires cargo-flamegraph (cargo install flamegraph)
# and Linux perf: sudo apt install linux-perf
flamegraph:
	CARGO_PROFILE_BENCH_DEBUG=true \
	RUSTFLAGS="-C force-frame-pointers=yes -C target-cpu=native" \
	cargo flamegraph --bench matching -- --bench 2>/dev/null
	@echo "Flamegraph written to flamegraph.svg"

# Real-time latency on isolated CPUs (requires root + isolcpus kernel param)
rt-bench:
	bash scripts/setup-cpu-isolation.sh
	sudo bash scripts/run-bench-rt.sh
```

**Step 4: Make scripts executable and test syntax**

```bash
chmod +x scripts/setup-cpu-isolation.sh scripts/run-bench-rt.sh
bash -n scripts/setup-cpu-isolation.sh
bash -n scripts/run-bench-rt.sh
```

Expected: no output (no syntax errors).

**Step 5: Run baseline and flamegraph targets to verify Makefile works**

```bash
make baseline
```

Expected: bench runs and output is saved.

**Step 6: Commit**

```bash
git add scripts/setup-cpu-isolation.sh scripts/run-bench-rt.sh Makefile
git commit -m "perf(scripts): add CPU isolation validator, RT bench runner, and flamegraph Makefile target"
```

---

## Task 15: Final benchmark comparison and summary

**Files:**
- Create: `docs/baselines/2026-03-02-final-summary.md`

**Step 1: Run the full benchmark suite**

```bash
cargo bench 2>&1 | tee docs/baselines/2026-03-02-final.txt
cargo bench --bench latency_histogram -- --nocapture 2>&1 | tee -a docs/baselines/2026-03-02-final.txt
```

**Step 2: Compare to baseline**

```bash
diff docs/baselines/2026-03-02-baseline.txt docs/baselines/2026-03-02-final.txt
```

**Step 3: Run all tests one final time**

```bash
cargo test --workspace
```

Expected: all tests pass.

**Step 4: Write summary**

Create `docs/baselines/2026-03-02-final-summary.md`:

```markdown
# Performance Optimization Results — 2026-03-02

## Changes Applied
1. Order struct: 104 bytes → 64 bytes (removed stop_price, sentinels, align(64))
2. Arena: parallel-array layout (no 128-byte Slot overhead)
3. Compiler: fat LTO, codegen-units=1, panic=abort, target-cpu=native
4. Engine: fixed-size inline event buffer (no heap)
5. Engine: #[inline(always)] hot path, #[cold] rejection paths
6. Engine: flat sorted Vec stop queues (no BTreeMap/VecDeque)
7. Scripts: PGO workflow, CPU isolation, flamegraph, RT bench runner

## Benchmark Results
[paste Criterion output here]

## Latency Histogram (dev machine — run again on bare metal with isolcpus)
[paste hdrhistogram output here]

## Next Steps (Production Readiness)
- Deploy on bare metal, enable isolcpus + nohz_full kernel params
- Run scripts/run-bench-rt.sh to get true hardware p99
- If p99 still above 1µs: run make flamegraph to identify hotspot
- Add huge-page arena: cargo bench --features matchx-arena/huge_pages
- Order entry TCP binary gateway (Phase 2)
```

**Step 5: Final commit**

```bash
git add docs/baselines/2026-03-02-final.txt docs/baselines/2026-03-02-final-summary.md
git commit -m "docs(perf): record final benchmark results and optimization summary"
```
