# Performance Micro-Optimization Design

**Date:** 2026-03-02
**Goals:** Sub-microsecond p99 latency on the hot path (order submission → trade event) on bare metal Linux.
**Approach:** Optimize the existing engine in place before adding networking — measure first, then fix structure, then tune compiler, then tune OS.

---

## Section 1 — Establish Baselines

Before touching any code, record current benchmark numbers as a regression gate.

**Changes:**
- Add `[profile.bench]` to workspace `Cargo.toml`: `opt-level = 3`, `codegen-units = 1`, `lto = "thin"`
- Add `.cargo/config.toml` with `RUSTFLAGS = "-C target-cpu=native"` for bench/release profiles
- Run `cargo bench` and commit output to `docs/baselines/2026-03-02-baseline.txt`

Every subsequent change must show improvement or parity against this baseline.

---

## Section 2 — Order Struct Layout (104 bytes → 64 bytes)

The `Order` struct is currently `#[repr(C)]` and approximately 104 bytes, spanning 1.625 cache lines. Every arena access that touches an `Order` risks two cache-line fetches.

**Root causes:**
- `Option<u64>` for `stop_price` → 16 bytes (bool discriminant + 7 padding + 8 value)
- `Option<u32>` for `stp_group` → 8 bytes
- `Option<ArenaIndex>` for `prev`/`next` → 8 bytes each
- Enum fields (`Side`, `OrderType`, `TimeInForce`) scattered between u64s causing 7-byte alignment padding gaps

**Changes:**
- Replace `Option<u64>` (stop_price) with sentinel `u64::MAX` — saves 8 bytes
- Replace `Option<u32>` (stp_group) with sentinel `u32::MAX` — saves 4 bytes
- Replace `Option<ArenaIndex>` (prev/next) with sentinel `ArenaIndex(u32::MAX)` — saves 8 bytes
- Reorder fields: all u64s first, then u32s, then u8s — eliminates alignment padding
- Add `#[repr(C, align(64))]` so each Order occupies exactly one cache line, preventing false sharing between adjacent arena slots
- Add compile-time guard: `const _: () = assert!(size_of::<Order>() == 64);`

All call sites using `Option::is_some()` / `.unwrap()` are replaced with sentinel comparisons. No semantic change.

---

## Section 3 — Compilation Profile Tuning

The hot path spans 4 crates (`matchx-types` → `matchx-arena` → `matchx-book` → `matchx-engine`). Without full LTO, the compiler cannot inline across crate boundaries.

**Changes:**
- `.cargo/config.toml`: `RUSTFLAGS = "-C target-cpu=native"` — enables AVX2, BMI2, POPCNT
- Workspace `[profile.release]` and `[profile.bench]`:
  - `lto = "fat"` — full cross-crate inlining
  - `codegen-units = 1` — single codegen unit for maximum optimization
  - `opt-level = 3`
  - `panic = "abort"` — removes unwinding machinery from hot paths
- PGO workflow script `scripts/pgo-bench.sh`:
  1. Instrument build: `RUSTFLAGS="-C instrument-coverage" cargo bench --no-run`
  2. Run instrumented binary to collect `*.profraw`
  3. Merge: `llvm-profdata merge -o merged.profdata *.profraw`
  4. Optimized build: `RUSTFLAGS="-C profile-use=merged.profdata" cargo bench`
  - Expected gain: 5–15% on top of LTO from better branch layout and inlining decisions

---

## Section 4 — Arena & Memory Tuning

The arena backing `Vec<Slot>` uses the default allocator (4KB pages). At 65,536 Order slots × 64 bytes = 4MB, this requires 1,024 TLB entries. A single 2MB huge page covers the entire arena, reducing TLB pressure to 2 entries.

**Changes:**
- Add `huge_pages` feature flag to `matchx-arena`:
  - When enabled: `mmap(MAP_ANONYMOUS | MAP_HUGETLB | MAP_HUGE_2MB)` backs the arena
  - Falls back to normal allocation if `MAP_HUGETLB` fails
  - Bare metal prerequisite: `echo 4 > /proc/sys/vm/nr_hugepages`
- Optional NUMA pinning: `mbind(MPOL_BIND)` after `mmap` to bind arena memory to the matching thread's NUMA node — prevents ~40ns cross-socket latency per cache miss on 2-socket machines
- Add `matchx-bench` benchmark for arena alloc+free round-trip to isolate TLB improvement

---

## Section 5 — Hot Path Micro-Optimizations

Targeted changes to eliminate hidden costs inside `MatchingEngine::process`.

**Changes:**

- **Fixed event buffer**: Replace `Vec<MatchEvent>` with `[MaybeUninit<MatchEvent>; 32]` + length counter. Removes bounds-check + realloc branch from `push` on every emit call. 32 slots is a safe upper bound for events from a single `process` call.

- **`#[inline(always)]` on `emit` and `remaining`**: Prevents inlining regressions if crate boundaries change in the future.

- **`#[cold]` on rejection paths**: `WouldCrossSpread`, `InsufficientLiquidity`, `DuplicateOrderId` handlers marked `#[cold]` — hints the branch predictor to optimize the accepted/filled path.

- **Stop-limit queue**: Replace `BTreeMap<u64, VecDeque<StopEntry>>` with a flat sorted `Vec<(u64, StopEntry)>`. Stop orders are rare; binary-search insert into a flat vec has better cache behavior than a heap-allocated tree. Trigger scan is a linear pass over a small set.

- **Compile-time size guards**: `const` assertions for `Order`, `PriceLevel`, and `MatchingEngine` sizes — prevents accidental struct bloat from future changes.

---

## Section 6 — System-Level Tuning & Profiling Workflow

Structural optimizations alone cannot guarantee sub-µs p99 if the OS scheduler interrupts the matching thread mid-operation.

**Changes:**

- **CPU isolation script `scripts/setup-cpu-isolation.sh`**: Documents and validates the kernel boot params:
  - `isolcpus=2,3` — removes CPUs 2–3 from the general scheduler
  - `nohz_full=2,3` — disables timer ticks on isolated cores (eliminates ~1µs periodic jitter)
  - `rcu_nocbs=2,3` — offloads RCU callbacks off isolated cores
  - Script validates these are active via `/sys/devices/system/cpu/isolated` before running benches

- **RT scheduling script `scripts/run-bench-rt.sh`**: Launches the benchmark binary under `chrt -f 99` (SCHED_FIFO priority 99)

- **Flamegraph workflow `make flamegraph`**:
  1. Compile with `-C force-frame-pointers=yes` + debug symbols
  2. `perf record -g ./bench-binary`
  3. Generate SVG via `cargo flamegraph`

- **`perf stat` checklist**: After each optimization round, verify improvement via:
  - `cache-misses`, `cache-references` — validates Order layout / huge page changes
  - `branch-misses` — validates `#[cold]` and `#[inline]` changes
  - `instructions`, `cycles` — overall efficiency

- **Latency histogram bench**: Add `latency_histogram` to `matchx-bench` using the `hdrhistogram` crate — records p50/p99/p999 per operation. This is the primary SLO measurement tool.

---

## Implementation Order

1. Baselines (Section 1) — no regressions possible; establishes the starting point
2. Order struct layout (Section 2) — highest structural leverage; validates immediately via size assert
3. Compilation tuning (Section 3) — multiplies gains from Section 2; no code changes required
4. Arena huge pages (Section 4) — isolated change, measurable via dedicated bench
5. Hot path micro-opts (Section 5) — fine-grained; each change benchmarked individually
6. System tuning (Section 6) — final layer; scripts + profiling harness for ongoing work

---

## Success Criteria

- `latency_histogram` bench shows p99 < 1µs on the crossing-trade path on bare metal with CPU isolation active
- `perf stat cache-misses` reduced by ≥50% vs baseline (validates Order layout + huge pages)
- All existing unit tests, property tests, and integration tests pass without modification
- `size_of::<Order>() == 64` enforced at compile time
