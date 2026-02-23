# Async WAL Matching Benchmark Comparison - 2026-03-03

Command used for both runs:

```bash
cargo bench -p matchx-bench --bench matching -- --sample-size 20
```

Criterion reports `time: [low estimate high]`.

| Benchmark | Before (feature branch) | Now (master after merge) | Delta (estimate) |
|---|---:|---:|---:|
| core_process_only | [5.3750, 5.3888, 5.4063] ns | [4.9577, 4.9664, 4.9749] ns | -7.84% |
| end_to_end_process_plus_enqueue | [932.40, 949.46, 967.36] ns | [912.77, 920.08, 926.17] ns | -3.10% |
| durability_lag_under_load | [923.06, 936.02, 945.77] ns | [909.03, 915.53, 920.70] ns | -2.19% |

Notes:
- Both runs were executed in the same session/machine context.
- Lower numbers are better.
- Deltas are computed from Criterion `estimate` values.
