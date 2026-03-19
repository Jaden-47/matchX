# Unsafe Code in matchX

This document catalogs every `unsafe` block in the codebase, its invariant,
and how it is validated.

## matchx-arena/src/lib.rs

### Arena::alloc (line ~55)
```rust
unsafe { self.data[idx as usize].as_mut_ptr().write(order) };
```
**Invariant:** `idx` was taken from the free list, so the slot is unoccupied.
**Validation:** Debug-mode generation counter (odd = occupied, even = free).
CI runs tests in debug mode. Miri validates in CI.

### Arena::free (line ~68)
```rust
unsafe { self.data[idx as usize].assume_init_drop() };
```
**Invariant:** Caller guarantees `idx` is occupied.
**Validation:** Debug-mode generation counter panics on double-free.

### Arena::get / get_mut (lines ~81, ~91)
```rust
unsafe { self.data[index.as_usize()].assume_init_ref() }
```
**Invariant:** Caller guarantees slot is occupied.
**Validation:** Debug-mode generation counter panics on use-after-free.

### unsafe impl Send for Arena
Arena holds no thread-local state and Order is Send.

## matchx-arena/src/lib.rs (feature = "huge_pages")

### HugePageArena::new — libc::mmap
Uses `libc::mmap` for 2MB huge page allocation with fallback to anonymous
mapping. `MAP_FAILED` is checked with `assert_ne!`. `munmap` called in Drop.
**Invariant:** Lifetime of `data` pointer matches `mmap_len`.

### HugePageArena alloc/free/get/get_mut
Same invariants as Arena. Uses raw pointer arithmetic (`data.add(idx)`)
instead of Vec indexing.

### unsafe impl Send for HugePageArena
Same rationale as Arena.

## matchx-engine/src/lib.rs

### Event buffer initialization (line ~58)
```rust
event_buf: unsafe { MaybeUninit::uninit().assume_init() }
```
**Invariant:** `[MaybeUninit<T>; N]` has no validity invariant. Every bit
pattern is valid for `MaybeUninit`, so an uninitialized array of `MaybeUninit`
is sound.
**Validation:** Compile-time static asserts verify:
- `size_of::<MaybeUninit<MatchEvent>>() == size_of::<MatchEvent>()`
- `align_of::<MaybeUninit<MatchEvent>>() == align_of::<MatchEvent>()`
- Buffer fits in 16KB stack frame
- `MAX_EVENTS_PER_CALL >= 64`

### Event buffer write (line ~82)
```rust
unsafe { self.event_buf[self.event_len].as_mut_ptr().write(event_fn(meta)); }
```
**Invariant:** `event_len < MAX_EVENTS_PER_CALL`.
**Validation:**
1. `process()` resets `event_len = 0` at entry
2. Worst-case single `process()` call emits <= 64 events (static assert)
3. `debug_assert!` guards in debug/test builds

### Event buffer read (line ~130)
```rust
unsafe { core::slice::from_raw_parts(self.event_buf.as_ptr() as *const MatchEvent, self.event_len) }
```
**Invariant:** The first `event_len` slots were written by `emit()`. The cast
from `*MaybeUninit<MatchEvent>` to `*MatchEvent` is sound because
`MaybeUninit<T>` is `#[repr(transparent)]` over `T` (verified by static asserts).

## Validation Strategy

- All tests run in debug mode (generation counters + debug_asserts active)
- `cargo +nightly miri test --workspace` runs in CI
- Property tests (proptest) exercise allocation/free patterns extensively
- Compile-time static asserts catch layout and capacity violations at build time
