# matchX Safety-First Layered Hardening Design

**Date**: 2026-03-30
**Status**: Approved
**Scope**: Full codebase hardening — safety, testing, performance, planned features, polish

## Context

matchX is a pre-production matching engine targeting sub-microsecond latency. A thorough repo audit identified **18 issues** across 7 crates (6 critical/high, 12 medium). The architecture is settled; the goal is to harden without major restructuring.

## Approach

**Safety-First Layered Hardening (Approach A)**: Fix correctness/safety first, then expand testing to catch regressions, then optimize performance, then implement planned features.

Each phase builds on the previous — tests catch regressions from safety fixes, perf work is validated by tests.

---

## Phase 0: Safety & Correctness Fixes (P0)

Six critical/high issues that block production.

### 0.1 Replace MaybeUninit Event Buffer

**File**: `matchx-engine/src/lib.rs:36-84`
**Problem**: Event buffer uses `MaybeUninit::uninit().assume_init()` — an array of uninitialized `MatchEvent`. Bounds checking is debug-only (`debug_assert!`). Reading the buffer in `process()` (line 129-134) uses raw `slice::from_raw_parts`.
**Fix**: Replace with `arrayvec::ArrayVec<MatchEvent, 64>`:
- Zero-heap allocation (stack-allocated like current approach)
- Bounds-checked push (`try_push` in release, `push` panics on overflow — detectable)
- Safe slice access via `Deref<[MatchEvent]>`
- Dependency: add `arrayvec = "0.7"` (no-std compatible)
**Impact**: Eliminates 3 unsafe blocks. ~0 perf regression (ArrayVec push is branchless on non-full).

### 0.2 Fix Stop-Trigger Cascading

**File**: `matchx-engine/src/lib.rs:535-587`
**Problem**: `drain_stop_triggers()` captures `last_trade_price` once at entry and uses it for all comparisons. When a triggered stop order fills at a new price (updating `last_trade_price` via `match_against_book` line 513), subsequent stops in the same drain pass are NOT re-evaluated against the new price.
**Example failure**: Stop A at 100, Stop B at 105. Trade at 100 triggers A → A's limit order fills at 110 → `last_trade_price = 110` → but B at 105 was already skipped because the drain loop broke at `105 > 100`.
**Fix**: Wrap drain in outer loop:
```rust
fn drain_stop_triggers(&mut self) {
    loop {
        let Some(last_price) = self.last_trade_price else { return };
        let prev_price = last_price;
        // ... drain bids where stop_px <= last_price ...
        // ... drain asks where stop_px >= last_price ...
        // If price didn't change, no new stops can trigger
        if self.last_trade_price == Some(prev_price) { break; }
    }
}
```
**Impact**: Correctness fix for cascading stop scenarios.

### 0.3 Arena Unsafe Hardening

**File**: `matchx-arena/src/lib.rs:55, 68, 81, 91`
**Problem**: `alloc()`, `free()`, `get()`, `get_mut()` assume caller invariants (slot is free/occupied) without runtime verification. Double-free or use-after-free would cause UB.
**Fix**:
- Add `#[cfg(debug_assertions)]` generation counter per slot: increment on alloc, check on free/get, detect stale references
- Add Miri to CI (see P1) to validate at test time
- Document invariants in `SAFETY.md` (see P4)
**Impact**: Zero runtime cost in release; catches bugs in debug/test.

### 0.4 Eliminate Hot-Path Panics in Book

**Files**: `matchx-book/src/lib.rs` (get_bid_level, get_ask_level called from engine)
**Problem**: `get_bid_level(price)` / `get_ask_level(price)` use `.expect("missing level")` — if called with a price that has no level (race between BBO query and level removal), engine panics.
**Fix**: Return `Option<&Level>` instead. Callers in engine treat `None` as "empty level, skip". This is already how BBO-based iteration should work.
**Impact**: Removes 2 potential panics from hot path.

### 0.5 Fenwick Underflow Protection

**File**: `matchx-book/src/lib.rs:39`
**Problem**: `checked_sub().expect("fenwick underflow")` panics if delta exceeds accumulated value.
**Fix**: Add `try_sub()` method returning `Result<(), UnderflowError>`. In release builds, saturate to 0 and log (non-fatal). In debug builds, panic to catch invariant violations early.
**Impact**: Prevents engine crash on Fenwick accounting bug.

### 0.6 DecrementAndCancel Validation

**File**: `matchx-engine/src/lib.rs:476`
**Problem**: `*remaining -= overlap` appears unbounded, but `overlap` is computed as `(*remaining).min(maker_remaining)` on line 462 — already bounded.
**Fix**: Add `debug_assert!(overlap <= *remaining)` to document the invariant.
**Impact**: Documentation only; confirms correctness.

---

## Phase 1: Testing Expansion (P1)

### 1.1 Missing Unit Tests

| Test | Target | Key assertions |
|------|--------|----------------|
| IOC partial fill | matchx-engine | Fill event for matched qty + Cancel event for remainder |
| IOC no match | matchx-engine | Immediate cancel, no fill |
| FOK reject | matchx-engine | Reject when available < requested |
| FOK fill | matchx-engine | Full fill when available >= requested |
| Modify cancel-replace | matchx-engine | Old order cancelled, new order placed; if crossing → match |
| Stop cascading | matchx-engine | Stop A triggers → fills → triggers Stop B |
| STP CancelOldest | matchx-engine | Resting order cancelled, taker continues matching |
| STP CancelBoth | matchx-engine | Both orders cancelled |
| STP DecrementAndCancel | matchx-engine | Both reduced by overlap, taker cancelled |
| Iceberg replenishment | matchx-engine | After visible slice consumed, next slice appears |
| Dense recentering | matchx-book | BBO drift past threshold triggers window shift |

### 1.2 Property Test Expansion

**File**: `matchx-engine/tests/properties.rs`

Expand `arbitrary_command` strategy to generate:
- Market, IOC, FOK order types (currently only Limit)
- STP group assignments (some orders share groups)
- Stop-limit orders with triggerable prices
- Modify commands referencing existing order IDs

New properties:
- **No self-trade fill**: When STP is active, no Fill event has `maker.stp_group == taker.stp_group`
- **Cancel references valid order**: Every OrderCancelled event references an ID that was previously Accepted
- **Fill quantity conservation**: Sum of fill quantities == original order qty - remaining qty

### 1.3 CI Additions

- **Miri**: Add `cargo +nightly miri test --workspace` job to CI workflow
- **Fuzzing harness**: Create `matchx-fuzz` crate with `cargo-fuzz` target:
  - Generates random `Vec<Command>` sequences
  - Feeds to engine, asserts: no panics, BBO never crosses after process(), fills conserve quantity
  - Run in CI with `--max-total-time=120` (2 min per PR)

---

## Phase 2: Performance Fixes (P2)

### 2.1 SparseVolumeIndex Incremental Fenwick Update

**File**: `matchx-book/src/lib.rs:85-96`
**Problem**: New sparse price → `Vec::insert` (O(N)) + `rebuild_fenwick()` (O(N)).
**Fix**: After `Vec::insert`, update only the affected Fenwick nodes (O(log N)) instead of full rebuild. Track max rank to avoid rebuilding beyond used portion.
**Impact**: O(N) → O(log N) per new sparse price.

### 2.2 Streaming Journal Reader

**File**: `matchx-journal/src/reader.rs:38`
**Problem**: `std::fs::read(path)` loads entire segment into memory.
**Fix**: Use `BufReader` with 64KB buffer. Decode records one at a time via streaming:
```rust
fn next_record(&mut self) -> Result<Option<(u64, Vec<u8>)>> {
    // Read 4-byte length header
    // Read sequence + payload + CRC
    // Validate CRC
    // Return (sequence, payload)
}
```
**Impact**: O(64KB) memory vs O(segment_size). Enables multi-GB segments.

### 2.3 VecDeque for Stop Lists

**File**: `matchx-engine/src/lib.rs:542, 567`
**Problem**: `stop_bids.remove(0)` and `stop_asks.remove(0)` are O(N) front-removal on Vec.
**Fix**: Change `stop_bids: Vec<(u64, StopEntry)>` to `VecDeque<(u64, StopEntry)>`. `pop_front()` is O(1).
**Note**: `partition_point` for sorted insertion still works on VecDeque slices.
**Impact**: O(N) → O(1) per stop trigger.

### 2.4 Symmetric Dense Recentering

**File**: `matchx-book/src/lib.rs:541-556`
**Problem**: Recentering checks are asymmetric (bid checks upper bound, ask checks lower bound independently).
**Fix**: Use unified condition: recenter when best_bid < base + margin OR best_ask > base + window - margin, with same margin for both sides.
**Impact**: Reduces spurious recentering under asymmetric order flow.

---

## Phase 3: Planned Feature Implementation (P3)

### 3.1 Hash-Chain Verification

**Crate**: matchx-journal
**Design**: Each record includes `prev_hash: [u8; 32]` — SHA-256 of the previous record's (length + sequence + payload + CRC). First record uses zero hash.
**Record format change**: `[u32 len][u64 seq][32B prev_hash][payload][u32 crc]`
**CRC now covers**: length + sequence + prev_hash + payload (fixes Issue #14 — header now included in CRC)
**Verification**: Reader validates chain on recovery; broken chain → truncate at break point.
**Dependency**: Add `sha2 = "0.10"` or use `ring` for hardware-accelerated SHA.

### 3.2 Snapshot Serialization

**New crate or module**: matchx-engine snapshot support
**Serialized state**:
- Arena: occupied slots + free list
- OrderBook: dense levels + sparse BTreeMap + occupancy bitset + Fenwick trees
- Engine: sequence, timestamp, last_trade_price, stop lists, config
**Format**: Length-prefixed binary (postcard or manual) with version header + CRC
**Recovery flow**: Load snapshot → replay journal from snapshot sequence → caught up
**Trigger**: Expose `engine.snapshot() -> Vec<u8>` and `Engine::from_snapshot(data) -> Result<Self>`

### 3.3 Segment Trailers

**Crate**: matchx-journal/codec
**Trailer record at end of each segment**:
```rust
struct SegmentTrailer {
    record_count: u64,
    min_sequence: u64,
    max_sequence: u64,
    hash_summary: [u8; 32],  // SHA-256 of all record hashes in segment
}
```
**Written on**: segment rotation (when max_segment_bytes reached)
**Used for**: Fast integrity check without reading every record; index for sequence-based seeking.

### 3.4 Fuzzing Harness

**New crate**: `matchx-fuzz`
**Targets**:
1. `fuzz_engine`: Random Command sequences → engine.process() → assert invariants
2. `fuzz_journal`: Random bytes → JournalReader::decode() → must not panic
3. `fuzz_codec`: Random bytes → codec::decode() → must return Err, never panic
**CI integration**: Run with `cargo fuzz run <target> -- -max_total_time=120`

---

## Phase 4: Polish (P4)

### 4.1 Clippy Fixes
- Collapse nested `if let` + `if` in engine (lines 274-285, 419-481)

### 4.2 API Documentation
- Add `///` doc comments on all public types, functions, and trait methods
- Focus on: what it does, panic conditions, safety requirements

### 4.3 SAFETY.md
- Document every `unsafe` block: location, invariant, proof sketch
- Reference Miri CI job for ongoing validation

### 4.4 CRC Header Fix
- Folded into P3.1 (hash-chain changes record format, includes length in CRC)

### 4.5 Makefile Targets
- `make lint` — runs `cargo clippy -- -D warnings`
- `make miri` — runs `cargo +nightly miri test --workspace`
- `make fuzz` — runs all fuzz targets for 2 minutes each

---

## Implementation Order

```
P0.1 (ArrayVec) → P0.2 (stop cascading) → P0.3 (arena debug checks) →
P0.4 (Option returns) → P0.5 (Fenwick try_sub) → P0.6 (debug_assert) →
P1.1 (unit tests) → P1.2 (property tests) → P1.3 (Miri + fuzz CI) →
P2.1 (sparse Fenwick) → P2.2 (streaming reader) → P2.3 (VecDeque stops) → P2.4 (recentering) →
P3.1 (hash-chain) → P3.2 (snapshots) → P3.3 (trailers) → P3.4 (fuzz targets) →
P4.1-P4.5 (polish)
```

## Non-Goals

- Major architectural changes (crate restructuring, new async runtime)
- New order types beyond what's already designed
- Cross-instrument support
- Wall-clock timestamp integration (deferred to compliance phase)
