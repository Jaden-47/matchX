# Latency Platform Async WAL Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add an async WAL pipeline and benchmark/regression framework so end-to-end command processing (`match + journal enqueue`) can target `P50 < 1us` and `P99 < 3us`.

**Architecture:** Keep `matchx-engine` matching semantics unchanged and deterministic. Advance `matchx-journal` with a bounded async append queue, background batch writer, durable watermark tracking, and segment-aware recovery. Extend `matchx-bench` and `matchx-itests` to measure/report end-to-end latency and validate replay determinism under async journaling.

**Tech Stack:** Rust 2024, `std::sync::mpsc::sync_channel` (bounded queue), `std::thread`, `crc32fast`, criterion, existing workspace crates (`matchx-engine`, `matchx-journal`, `matchx-itests`, `matchx-bench`)

**Execution Requirements:** Follow @test-driven-development and @verification-before-completion for every task. Use @requesting-code-review after implementation.

---

### Task 1: Worktree + Baseline Verification

**Files:**
- Modify: none
- Test: none

**Step 1: Create isolated worktree**

Run:
```bash
git worktree add .worktrees/latency-platform-async-wal -b feat/latency-platform-async-wal
```

Expected: new worktree and branch created.

**Step 2: Enter worktree**

Run:
```bash
cd .worktrees/latency-platform-async-wal
```

Expected: shell in isolated worktree.

**Step 3: Run baseline tests**

Run:
```bash
cargo test
```

Expected: all existing tests pass.

**Step 4: Run baseline benchmark compile**

Run:
```bash
cargo bench -p matchx-bench --bench matching --no-run
```

Expected: benchmark target compiles.

**Step 5: Commit setup note**

Run:
```bash
git commit --allow-empty -m "chore: start async WAL latency platform implementation branch"
```

---

### Task 2: Define Async Journal API Surface

**Files:**
- Create: `crates/matchx-journal/src/async_journal.rs`
- Modify: `crates/matchx-journal/src/lib.rs`
- Test: `crates/matchx-journal/src/async_journal.rs` (inline `#[cfg(test)]`)

**Step 1: Write failing API-shape tests**

Add test skeleton:
```rust
#[test]
fn async_journal_exposes_accepted_and_durable_sequence() {
    let j = AsyncJournal::open(tempdir.path().join("seg"), AsyncJournalConfig::default()).unwrap();
    assert_eq!(j.accepted_sequence(), 0);
    assert_eq!(j.durable_sequence(), 0);
}
```

**Step 2: Run test to verify it fails**

Run:
```bash
cargo test -p matchx-journal async_journal_exposes_accepted_and_durable_sequence -v
```

Expected: FAIL with unresolved `AsyncJournal` / `AsyncJournalConfig`.

**Step 3: Add minimal API types**

Implement:
```rust
pub struct AsyncJournalConfig {
    pub queue_capacity: usize,
    pub batch_size: usize,
    pub flush_interval_ms: u64,
    pub segment_max_bytes: u64,
}

pub struct AsyncJournal { /* fields hidden for now */ }
impl AsyncJournal {
    pub fn open(path_prefix: impl AsRef<Path>, cfg: AsyncJournalConfig) -> Result<Self, JournalError> { ... }
    pub fn accepted_sequence(&self) -> u64 { ... }
    pub fn durable_sequence(&self) -> u64 { ... }
}
```

**Step 4: Run tests to verify pass**

Run:
```bash
cargo test -p matchx-journal async_journal_exposes_accepted_and_durable_sequence -v
```

Expected: PASS.

**Step 5: Commit**

Run:
```bash
git add crates/matchx-journal/src/lib.rs crates/matchx-journal/src/async_journal.rs
git commit -m "feat(journal): add async journal API surface and config type"
```

---

### Task 3: Add Bounded Queue Append + Backpressure Errors

**Files:**
- Modify: `crates/matchx-journal/src/async_journal.rs`
- Modify: `crates/matchx-journal/src/lib.rs`
- Test: `crates/matchx-journal/src/async_journal.rs` (inline tests)

**Step 1: Write failing queue-full behavior test**

Add test:
```rust
#[test]
fn append_returns_queue_full_when_capacity_exhausted() {
    let cfg = AsyncJournalConfig { queue_capacity: 1, ..Default::default() };
    let j = AsyncJournal::open(prefix, cfg).unwrap();
    j.append(1, &cmd()).unwrap();
    let err = j.append(2, &cmd()).unwrap_err();
    assert!(matches!(err, JournalError::QueueFull));
}
```

**Step 2: Run test to verify it fails**

Run:
```bash
cargo test -p matchx-journal append_returns_queue_full_when_capacity_exhausted -v
```

Expected: FAIL (missing `append`/`QueueFull` behavior).

**Step 3: Implement minimal append path**

Implement:
```rust
pub fn append(&self, sequence: u64, cmd: &Command) -> Result<(), JournalError> {
    self.tx.try_send(AppendRecord { sequence, bytes: encode_record(sequence, cmd) })
        .map_err(|e| match e {
            TrySendError::Full(_) => JournalError::QueueFull,
            TrySendError::Disconnected(_) => JournalError::WriterStopped,
        })?;
    self.accepted.store(sequence, Ordering::Release);
    Ok(())
}
```

**Step 4: Run focused tests**

Run:
```bash
cargo test -p matchx-journal append_returns_queue_full_when_capacity_exhausted -v
```

Expected: PASS.

**Step 5: Commit**

Run:
```bash
git add crates/matchx-journal/src/lib.rs crates/matchx-journal/src/async_journal.rs
git commit -m "feat(journal): add bounded async append with queue full backpressure"
```

---

### Task 4: Factor Record Framing Into Reusable Codec Helpers

**Files:**
- Modify: `crates/matchx-journal/src/codec.rs`
- Modify: `crates/matchx-journal/src/writer.rs`
- Test: `crates/matchx-journal/src/codec.rs` (inline tests)

**Step 1: Write failing framing roundtrip test**

Add test:
```rust
#[test]
fn framed_record_roundtrips_command_and_sequence() {
    let bytes = encode_record(7, &sample_new_order());
    let (seq, cmd, used) = decode_record(&bytes).unwrap();
    assert_eq!(seq, 7);
    assert_eq!(cmd, sample_new_order());
    assert_eq!(used, bytes.len());
}
```

**Step 2: Run test to verify it fails**

Run:
```bash
cargo test -p matchx-journal framed_record_roundtrips_command_and_sequence -v
```

Expected: FAIL with missing framing helpers.

**Step 3: Implement framing helpers**

Add in `codec.rs`:
```rust
pub fn encode_record(sequence: u64, cmd: &Command) -> Vec<u8> { ... } // len + seq + payload + crc
pub fn decode_record(data: &[u8]) -> Result<(u64, Command, usize), JournalError> { ... }
```

Refactor `JournalWriter::append` to call `encode_record`.

**Step 4: Run tests**

Run:
```bash
cargo test -p matchx-journal framed_record_roundtrips_command_and_sequence -v
cargo test -p matchx-journal write_and_read_back_commands -v
```

Expected: PASS.

**Step 5: Commit**

Run:
```bash
git add crates/matchx-journal/src/codec.rs crates/matchx-journal/src/writer.rs
git commit -m "refactor(journal): share record framing helpers across writer paths"
```

---

### Task 5: Implement Background Writer Thread + Durable Watermark

**Files:**
- Modify: `crates/matchx-journal/src/async_journal.rs`
- Modify: `crates/matchx-journal/src/writer.rs`
- Test: `crates/matchx-journal/src/async_journal.rs` (inline tests)

**Step 1: Write failing durability progression test**

Add test:
```rust
#[test]
fn durable_sequence_catches_up_after_background_flush() {
    let j = AsyncJournal::open(prefix, AsyncJournalConfig::default()).unwrap();
    j.append(1, &cmd()).unwrap();
    j.append(2, &cmd()).unwrap();
    wait_until(|| j.durable_sequence() >= 2, Duration::from_millis(250));
    assert_eq!(j.accepted_sequence(), 2);
}
```

**Step 2: Run test to verify it fails**

Run:
```bash
cargo test -p matchx-journal durable_sequence_catches_up_after_background_flush -v
```

Expected: FAIL (no writer thread / durable watermark updates).

**Step 3: Implement writer worker**

Implement minimal worker:
```rust
thread::spawn(move || {
    let mut writer = JournalWriter::open(&segment_path)?;
    loop {
        // recv first record, drain up to batch_size via try_recv, write batch
        writer.append_raw_batch(&batch)?;
        writer.flush_if_due()?;
        durable.store(last_seq, Ordering::Release);
    }
});
```

Also add explicit `close()` / `Drop` join semantics so tests can shutdown cleanly.

**Step 4: Run tests**

Run:
```bash
cargo test -p matchx-journal durable_sequence_catches_up_after_background_flush -v
```

Expected: PASS.

**Step 5: Commit**

Run:
```bash
git add crates/matchx-journal/src/async_journal.rs crates/matchx-journal/src/writer.rs
git commit -m "feat(journal): add async WAL background writer and durable watermark"
```

---

### Task 6: Implement Degraded Mode On Writer Failure

**Files:**
- Modify: `crates/matchx-journal/src/async_journal.rs`
- Modify: `crates/matchx-journal/src/lib.rs`
- Test: `crates/matchx-journal/src/async_journal.rs`

**Step 1: Write failing degraded-state test**

Add test:
```rust
#[test]
fn append_fails_fast_after_writer_enters_degraded_state() {
    let j = AsyncJournal::open(invalid_prefix(), AsyncJournalConfig::default()).unwrap();
    // force writer error (e.g. rotate into non-creatable path)
    trigger_writer_io_error(&j);
    wait_until(|| j.is_degraded(), Duration::from_millis(250));
    assert!(matches!(j.append(10, &cmd()), Err(JournalError::WriterDegraded)));
}
```

**Step 2: Run test to verify it fails**

Run:
```bash
cargo test -p matchx-journal append_fails_fast_after_writer_enters_degraded_state -v
```

Expected: FAIL (no degraded flag/error).

**Step 3: Add degraded state propagation**

Implement:
```rust
if self.degraded.load(Ordering::Acquire) {
    return Err(JournalError::WriterDegraded);
}
```

Writer thread sets degraded flag on any persistent I/O failure.

**Step 4: Run tests**

Run:
```bash
cargo test -p matchx-journal append_fails_fast_after_writer_enters_degraded_state -v
```

Expected: PASS.

**Step 5: Commit**

Run:
```bash
git add crates/matchx-journal/src/lib.rs crates/matchx-journal/src/async_journal.rs
git commit -m "feat(journal): fail fast with degraded mode after async writer errors"
```

---

### Task 7: Add Segment Rotation And Multi-Segment Reader

**Files:**
- Modify: `crates/matchx-journal/src/writer.rs`
- Modify: `crates/matchx-journal/src/reader.rs`
- Modify: `crates/matchx-journal/src/lib.rs`
- Test: `crates/matchx-journal/src/lib.rs` (or new `crates/matchx-journal/tests/segments.rs`)

**Step 1: Write failing rotation test**

Add test:
```rust
#[test]
fn rotates_segments_when_max_bytes_exceeded() {
    let mut w = JournalWriter::open_segmented(dir.path(), 256).unwrap();
    for seq in 1..=200 { w.append(seq, &cmd()).unwrap(); }
    let segs = list_segments(dir.path());
    assert!(segs.len() > 1);
}
```

**Step 2: Run test to verify it fails**

Run:
```bash
cargo test -p matchx-journal rotates_segments_when_max_bytes_exceeded -v
```

Expected: FAIL (single-file writer only).

**Step 3: Implement rotation + reader scan**

Implement segmented format:
```rust
journal-00000001.wal
journal-00000002.wal
```

Reader loads segments in lexical order and yields sequential `JournalEntry`.

**Step 4: Run tests**

Run:
```bash
cargo test -p matchx-journal rotates_segments_when_max_bytes_exceeded -v
cargo test -p matchx-journal write_and_read_back_commands -v
```

Expected: PASS.

**Step 5: Commit**

Run:
```bash
git add crates/matchx-journal/src/writer.rs crates/matchx-journal/src/reader.rs crates/matchx-journal/src/lib.rs
git commit -m "feat(journal): add WAL segment rotation and multi-segment replay reader"
```

---

### Task 8: Add Torn-Tail Truncation Recovery

**Files:**
- Modify: `crates/matchx-journal/src/reader.rs`
- Create: `crates/matchx-journal/src/recovery.rs`
- Modify: `crates/matchx-journal/src/lib.rs`
- Test: `crates/matchx-journal/tests/recovery.rs`

**Step 1: Write failing torn-tail recovery test**

Add test:
```rust
#[test]
fn recovery_truncates_to_last_valid_record_boundary() {
    write_n_records(&path, 10);
    corrupt_tail_bytes(&path);
    let report = RecoveryManager::recover_path(&path).unwrap();
    assert_eq!(report.last_valid_sequence, 9);
    assert!(report.truncated_bytes > 0);
}
```

**Step 2: Run test to verify it fails**

Run:
```bash
cargo test -p matchx-journal recovery_truncates_to_last_valid_record_boundary -v
```

Expected: FAIL (no recovery manager).

**Step 3: Implement recovery manager**

Implement:
```rust
pub struct RecoveryReport { pub last_valid_sequence: u64, pub truncated_bytes: u64 }
pub struct RecoveryManager;
impl RecoveryManager {
    pub fn recover_dir(dir: &Path) -> Result<RecoveryReport, JournalError> { ... }
}
```

Use `decode_record` sequential scan and `set_len(last_valid_offset)` on corruption.

**Step 4: Run tests**

Run:
```bash
cargo test -p matchx-journal recovery_truncates_to_last_valid_record_boundary -v
cargo test -p matchx-journal -v
```

Expected: PASS.

**Step 5: Commit**

Run:
```bash
git add crates/matchx-journal/src/recovery.rs crates/matchx-journal/src/reader.rs crates/matchx-journal/src/lib.rs crates/matchx-journal/tests/recovery.rs
git commit -m "feat(journal): add torn-tail truncation recovery manager"
```

---

### Task 9: Build End-to-End Pipeline Adapter (Engine + Async Journal)

**Files:**
- Create: `crates/matchx-bench/src/pipeline.rs`
- Modify: `crates/matchx-bench/src/lib.rs`
- Modify: `crates/matchx-bench/Cargo.toml`
- Test: `crates/matchx-bench/src/pipeline.rs` (inline tests)

**Step 1: Write failing pipeline append test**

Add test:
```rust
#[test]
fn pipeline_processes_command_and_enqueues_wal_record() {
    let mut p = EndToEndPipeline::new(test_config(), 1024, journal_cfg(), prefix).unwrap();
    let events = p.process(Command::CancelOrder { id: OrderId(42) }).unwrap();
    assert!(!events.is_empty());
    assert!(p.accepted_sequence() >= 1);
}
```

**Step 2: Run test to verify it fails**

Run:
```bash
cargo test -p matchx-bench pipeline_processes_command_and_enqueues_wal_record -v
```

Expected: FAIL (missing pipeline).

**Step 3: Implement minimal adapter**

Create:
```rust
pub struct EndToEndPipeline { engine: MatchingEngine, journal: AsyncJournal, input_seq: u64 }
impl EndToEndPipeline {
    pub fn process(&mut self, cmd: Command) -> Result<Vec<MatchEvent>, JournalError> {
        self.input_seq += 1;
        let out = self.engine.process(cmd.clone()).to_vec();
        self.journal.append(self.input_seq, &cmd)?;
        Ok(out)
    }
}
```

**Step 4: Run tests**

Run:
```bash
cargo test -p matchx-bench pipeline_processes_command_and_enqueues_wal_record -v
```

Expected: PASS.

**Step 5: Commit**

Run:
```bash
git add crates/matchx-bench/src/lib.rs crates/matchx-bench/src/pipeline.rs crates/matchx-bench/Cargo.toml
git commit -m "feat(bench): add end-to-end pipeline adapter with async WAL enqueue"
```

---

### Task 10: Add Async-WAL Replay Determinism Integration Test

**Files:**
- Modify: `crates/matchx-itests/Cargo.toml`
- Create: `crates/matchx-itests/tests/async_wal_replay_determinism.rs`

**Step 1: Write failing integration test**

Add test:
```rust
#[test]
fn replay_matches_original_output_with_async_wal() {
    // run command stream through pipeline (engine + async wal)
    // wait until durable sequence catches up
    // replay durable WAL into fresh engine
    // assert emitted MatchEvent vectors are identical
}
```

**Step 2: Run test to verify it fails**

Run:
```bash
cargo test -p matchx-itests replay_matches_original_output_with_async_wal -v
```

Expected: FAIL until async WAL reader path is wired in test.

**Step 3: Implement minimal test harness logic**

Use existing `JournalReader` + new segment/recovery paths to load commands in order.

**Step 4: Run tests**

Run:
```bash
cargo test -p matchx-itests replay_matches_original_output_with_async_wal -v
```

Expected: PASS.

**Step 5: Commit**

Run:
```bash
git add crates/matchx-itests/Cargo.toml crates/matchx-itests/tests/async_wal_replay_determinism.rs
git commit -m "test(itests): verify replay determinism under async WAL persistence"
```

---

### Task 11: Expand Benchmarks For Core vs End-to-End + Durability Lag

**Files:**
- Modify: `crates/matchx-bench/benches/matching.rs`
- Modify: `crates/matchx-bench/src/lib.rs`
- Create: `crates/matchx-bench/src/metrics.rs`

**Step 1: Write failing benchmark compile changes**

Add benchmark entries (initially placeholders):
```rust
fn bench_core_process_only(c: &mut Criterion) { ... }
fn bench_end_to_end_process_plus_enqueue(c: &mut Criterion) { ... }
fn bench_durability_lag_under_load(c: &mut Criterion) { ... }
```

**Step 2: Run bench compile to verify failures**

Run:
```bash
cargo bench -p matchx-bench --bench matching --no-run
```

Expected: FAIL until missing helpers are implemented.

**Step 3: Implement benchmark helpers + latency summary output**

Implement lightweight percentile summarizer in `metrics.rs` for lag/latency samples and print p50/p95/p99/p999.

**Step 4: Run benchmark compile and a short sample**

Run:
```bash
cargo bench -p matchx-bench --bench matching --no-run
cargo bench -p matchx-bench --bench matching -- --sample-size 20
```

Expected: compile passes; benchmark executes and prints named benchmark groups.

**Step 5: Commit**

Run:
```bash
git add crates/matchx-bench/benches/matching.rs crates/matchx-bench/src/lib.rs crates/matchx-bench/src/metrics.rs
git commit -m "feat(bench): add core vs e2e latency benchmarks and durability lag metrics"
```

---

### Task 12: Add Performance Regression Gate Script + CI Hook

**Files:**
- Create: `.github/scripts/check-latency-regression.sh`
- Modify: `.github/workflows/ci.yml`
- Create: `docs/plans/latency-benchmark-baseline.md`

**Step 1: Write failing CI check command**

Add script invocation in workflow before script exists.

**Step 2: Run local shellcheck/execute to confirm failure**

Run:
```bash
bash .github/scripts/check-latency-regression.sh
```

Expected: FAIL (`No such file or directory`).

**Step 3: Implement script and baseline contract**

Script behavior:
```bash
# run selected bench
# parse Criterion JSON estimates
# compare p99 deltas against baseline threshold (default 10%)
# exit 1 on regression
```

Document baseline file format in `docs/plans/latency-benchmark-baseline.md`.

**Step 4: Run validation**

Run:
```bash
bash .github/scripts/check-latency-regression.sh
```

Expected: PASS locally with current baseline (or explicit informational skip if baseline absent).

**Step 5: Commit**

Run:
```bash
git add .github/scripts/check-latency-regression.sh .github/workflows/ci.yml docs/plans/latency-benchmark-baseline.md
git commit -m "ci(perf): add p99 latency regression gate for async WAL benchmarks"
```

---

### Task 13: Final Verification And Handoff

**Files:**
- Modify: none
- Test: full workspace

**Step 1: Run format check**

Run:
```bash
cargo fmt --all -- --check
```

Expected: PASS.

**Step 2: Run lint**

Run:
```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: PASS.

**Step 3: Run tests**

Run:
```bash
cargo test --workspace
```

Expected: PASS.

**Step 4: Run benchmark compile check**

Run:
```bash
cargo bench -p matchx-bench --bench matching --no-run
```

Expected: PASS.

**Step 5: Commit verification marker**

Run:
```bash
git commit --allow-empty -m "chore: verify async WAL latency platform implementation"
```

---

## Notes For Executor

- Keep commits small and task-scoped; do not batch multiple tasks into one commit.
- Preserve deterministic replay contract: stable sequence ordering, CRC validation, canonical decode path.
- Do not introduce blocking disk waits in hot path code.
- If queue backpressure behavior changes (reject vs block), update tests and design docs in the same task commit.
