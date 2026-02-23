# Latency Benchmark Baseline Contract

The CI latency gate reads a JSON baseline file and fails when observed `p99` latency regresses beyond a threshold.

## Gate Command

```bash
bash .github/scripts/check-latency-regression.sh
```

Environment variables:

- `LATENCY_BASELINE_FILE` (default: `.github/latency-benchmark-baseline.json`)
- `LATENCY_REGRESSION_THRESHOLD_PCT` (default: `10`)
- `LATENCY_SAMPLE_SIZE` (default: `20`)

If the baseline file is missing, the script exits `0` with an informational skip message.

## Baseline JSON Format

Create `.github/latency-benchmark-baseline.json`:

```json
{
  "schema_version": 1,
  "unit": "ns",
  "benchmarks": {
    "core_process_only": { "p99": 60 },
    "end_to_end_process_plus_enqueue": { "p99": 140000 },
    "durability_lag_under_load": { "p99": 23000 }
  }
}
```

Notes:

- Benchmark names must match emitted names from `crates/matchx-bench/benches/matching.rs`.
- `p99` values are in nanoseconds.
- You can use either object (`{ "p99": 123 }`) or plain numeric value (`123`) per benchmark.

## Pass/Fail Rule

For each configured benchmark:

```text
allowed_p99 = baseline_p99 * (1 + threshold_pct / 100)
```

The gate fails when:

- benchmark output for a configured benchmark is missing, or
- `current_p99 > allowed_p99`

The script also reads Criterion `target/criterion/<benchmark>/new/estimates.json` and reports `mean.point_estimate` for context in CI logs.
