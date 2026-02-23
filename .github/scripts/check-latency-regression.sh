#!/usr/bin/env bash
set -euo pipefail

BASELINE_FILE="${LATENCY_BASELINE_FILE:-.github/latency-benchmark-baseline.json}"
THRESHOLD_PCT="${LATENCY_REGRESSION_THRESHOLD_PCT:-10}"
SAMPLE_SIZE="${LATENCY_SAMPLE_SIZE:-20}"

if [[ ! -f "${BASELINE_FILE}" ]]; then
  echo "[latency-gate] baseline file not found at ${BASELINE_FILE}; skipping regression check."
  echo "[latency-gate] create baseline JSON (see docs/plans/latency-benchmark-baseline.md) to enable enforcement."
  exit 0
fi

BENCH_LOG="$(mktemp)"
cleanup() {
  rm -f "${BENCH_LOG}"
}
trap cleanup EXIT

echo "[latency-gate] running benchmark suite (sample-size=${SAMPLE_SIZE})..."
cargo bench -p matchx-bench --bench matching -- --sample-size "${SAMPLE_SIZE}" 2>&1 | tee "${BENCH_LOG}"

python3 - "${BASELINE_FILE}" "${THRESHOLD_PCT}" "${BENCH_LOG}" <<'PY'
import json
import pathlib
import re
import sys

baseline_path = pathlib.Path(sys.argv[1])
threshold_pct = float(sys.argv[2])
log_path = pathlib.Path(sys.argv[3])

with baseline_path.open("r", encoding="utf-8") as f:
    baseline = json.load(f)

benchmarks = baseline.get("benchmarks", {})
if not benchmarks:
    print(f"[latency-gate] no benchmarks configured in {baseline_path}; skipping.")
    sys.exit(0)

log_text = log_path.read_text(encoding="utf-8")
pattern = re.compile(r"^\[bench\]\s+([a-zA-Z0-9_]+):.*\bp99=([0-9]+)ns\b", re.MULTILINE)
observed = {}
for name, p99 in pattern.findall(log_text):
    observed[name] = int(p99)

if not observed:
    print("[latency-gate] no p99 benchmark summaries found in bench output; failing.")
    sys.exit(1)

regressions = []
print("[latency-gate] evaluating p99 regression thresholds...")
for name, expected_cfg in benchmarks.items():
    if isinstance(expected_cfg, dict):
        baseline_p99 = expected_cfg.get("p99")
    else:
        baseline_p99 = expected_cfg

    if baseline_p99 is None:
        print(f"  - {name}: missing baseline p99, skipping.")
        continue

    if name not in observed:
        print(f"  - {name}: not present in current run; failing.")
        regressions.append((name, baseline_p99, None, None))
        continue

    current_p99 = observed[name]
    allowed_p99 = baseline_p99 * (1.0 + threshold_pct / 100.0)
    mean_estimate = None
    estimate_path = pathlib.Path("target") / "criterion" / name / "new" / "estimates.json"
    if estimate_path.exists():
        with estimate_path.open("r", encoding="utf-8") as f:
            estimate_json = json.load(f)
        mean_estimate = estimate_json.get("mean", {}).get("point_estimate")

    status = "OK"
    if current_p99 > allowed_p99:
        status = "REGRESSION"
        regressions.append((name, baseline_p99, current_p99, allowed_p99))

    mean_suffix = ""
    if mean_estimate is not None:
        mean_suffix = f", criterion_mean={int(mean_estimate)}ns"
    print(
        f"  - {name}: baseline_p99={int(baseline_p99)}ns, "
        f"current_p99={int(current_p99)}ns, allowed_p99={int(allowed_p99)}ns"
        f"{mean_suffix} => {status}"
    )

if regressions:
    print("[latency-gate] regression detected:")
    for name, baseline_p99, current_p99, allowed_p99 in regressions:
        if current_p99 is None:
            print(f"  - {name}: missing in current benchmark output (baseline={baseline_p99}ns)")
        else:
            print(
                f"  - {name}: current p99 {current_p99}ns exceeds allowed {int(allowed_p99)}ns "
                f"(baseline {baseline_p99}ns, threshold {threshold_pct:.1f}%)"
            )
    sys.exit(1)

print("[latency-gate] all configured p99 checks are within threshold.")
PY
