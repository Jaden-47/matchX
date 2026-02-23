# Latency Platform Design (Async WAL + End-to-End SLO)

**Date:** 2026-03-02  
**Status:** Approved  
**Scope:** Advance low-latency end-to-end command path by upgrading journaling and performance validation

## 1. Context

The project already has a mature single-shard matching core (`matchx-book`, `matchx-engine`) with deterministic replay tests and broad order-type coverage. The next highest-leverage module is the persistence + measurement layer:

- `matchx-journal` currently uses direct append/flush and limited recovery scope.
- `matchx-bench` has partial benchmark coverage and no strict latency regression gates.

The user-selected direction is:

- Optimize for best latency.
- Measure end-to-end command path.
- Use async WAL durability semantics (accept possible recent-command loss on crash).

## 2. Goals And Non-Goals

### Goals

- Achieve end-to-end latency target:
  - `P50 < 1us`
  - `P99 < 3us`
- Define end-to-end critical path as: command entry -> engine output + journal enqueue completed.
- Keep matching behavior deterministic and replay-consistent.
- Add repeatable benchmarks and regression gates to prevent performance drift.

### Non-Goals

- No shift to strict per-command `fsync`.
- No multi-shard architecture work in this phase.
- No new matching semantics or order-type changes.

## 3. Chosen Approach

Recommended approach selected: build a **latency platform module** spanning `matchx-journal` + `matchx-bench`:

1. Introduce asynchronous journaling with background disk writer.
2. Keep hot path non-blocking on disk.
3. Benchmark exactly what is promised (end-to-end enqueue latency, not only pure matching).
4. Add durable-sequence observability to quantify loss window and writer lag.

This balances immediate latency gain with correctness visibility.

## 4. Architecture

### 4.1 Data Path

1. Command arrives.
2. `MatchingEngine::process()` computes events.
3. Journal record is encoded and appended to bounded in-memory queue (`AsyncJournal::append`).
4. Caller returns immediately after successful enqueue.
5. Background writer drains queue in batches, appends to WAL segments, flushes per configured policy, and advances durable watermark.

### 4.2 Sequences

- `accepted_sequence`: highest sequence accepted/enqueued by hot path.
- `durable_sequence`: highest sequence confirmed persisted by writer.
- `durability_lag = accepted_sequence - durable_sequence`.

### 4.3 Module Responsibilities

- `matchx-engine`:
  - Preserve matching logic.
  - Integrate non-blocking journal append after command processing.
  - Surface append result and backpressure/errors to caller.

- `matchx-journal`:
  - Add `AsyncJournal` producer API.
  - Add `WalWriter` background worker.
  - Support segment rotation and orderly shutdown.
  - Retain deterministic decode/replay ordering.

- `matchx-bench`:
  - Measure core-only and end-to-end modes separately.
  - Report latency percentiles and throughput.
  - Track durability lag under sustained load.

## 5. Error Handling And Failure Semantics

### 5.1 Queue Full

- Bounded queue is required for predictable latency.
- On full queue, return explicit `Backpressure`/`QueueFull`.
- Do not block hot path waiting for disk.

### 5.2 Writer Failures

- On disk/IO error, writer transitions journal to `degraded` state.
- New appends fail fast with explicit error.
- Health state exposed to intake/control plane.

### 5.3 Crash Behavior (Accepted Trade-off)

- Entries not yet durable may be lost on crash.
- Recovery starts from last valid durable boundary.

### 5.4 Corruption / Torn Tail

- Keep per-record CRC validation.
- On recovery, scan sequentially and truncate at first invalid/torn record.

### 5.5 Determinism Constraints

- Persist records in input sequence order.
- Replay uses canonical decode and deterministic command processing path.
- No wall-clock dependency in matching output sequence semantics.

## 6. Testing Strategy

### 6.1 Correctness

- Replay equivalence with async WAL enabled.
- Sequence monotonicity and no write reordering.
- Fault injection:
  - queue full
  - forced writer I/O failure
  - torn/corrupt tail recovery

### 6.2 Performance

- Microbench: `engine.process()` only.
- End-to-end bench: `process + async journal enqueue`.
- Durability lag bench: load test with tracked `durability_lag` distribution.

### 6.3 Regression Gates

- Store benchmark baselines.
- Gate CI on controlled benchmark profile.
- Fail if p99 regresses beyond configured budget (for example 10%) for key workloads.

## 7. Success Criteria

- End-to-end latency (`process + enqueue`) meets:
  - `P50 < 1us`
  - `P99 < 3us`
- No replay divergence under stress tests.
- Durability lag is bounded, observable, and documented for configured writer policy.

## 8. Rollout Notes

- Start behind feature/config flag for async journal mode.
- Keep synchronous path available for comparison during validation.
- Promote async path to default only after benchmark and fault-injection sign-off.
