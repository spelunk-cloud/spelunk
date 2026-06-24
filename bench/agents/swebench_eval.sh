#!/usr/bin/env bash
# bench/agents/swebench_eval.sh — run SWE-bench Docker evaluation
#
# Converts agent patches to SWE-bench prediction format, runs the
# official Docker harness, and merges resolve data back into the
# result JSON with explicit denominator.
#
# Prerequisites:
#   - SWE-bench harness installed: pip install swebench
#   - Docker images pulled for the target dataset
#   - Agent patches saved via agent.py --save-patch
#
# Options:
#   --results FILE     batch result JSON from agent run
#   --patches-dir DIR  directory with per-task .patch files
#   --dataset NAME     HuggingFace dataset (default: princeton-nlp/SWE-bench)
#   --split NAME       dataset split (default: test)
#   --max-workers N    parallel eval workers (default: 4)
#   --timeout SEC      per-instance timeout (default: 900)
#
# NOTE: We default to the full SWE-bench split (not SWE-bench_Verified) because all
# 50 tasks in tasks_50.json exist in the full split, whereas only 24/50 are in
# SWE-bench_Verified — causing a 26-task denominator gap. See issue #252.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

RESULTS=""
PATCHES_DIR=""
DATASET="princeton-nlp/SWE-bench"
SPLIT="test"
MAX_WORKERS=4
TIMEOUT=900

usage() {
    grep '^#' "$0" | grep -v '#!/' | sed 's/^# \?//'
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --results)      RESULTS="$2";      shift 2 ;;
        --patches-dir)  PATCHES_DIR="$2";   shift 2 ;;
        --dataset)      DATASET="$2";       shift 2 ;;
        --split)        SPLIT="$2";         shift 2 ;;
        --max-workers)  MAX_WORKERS="$2";   shift 2 ;;
        --timeout)      TIMEOUT="$2";       shift 2 ;;
        -h|--help)      usage ;;
        *) echo "Unknown argument: $1" >&2; usage ;;
    esac
done

if [[ -z "$RESULTS" || -z "$PATCHES_DIR" ]]; then
    echo "Error: --results and --patches-dir are required." >&2; usage
fi

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
PREDICTIONS_FILE="${SCRIPT_DIR}/../predictions/eval-${TIMESTAMP}.json"
mkdir -p "$(dirname "$PREDICTIONS_FILE")"

echo "=== SWE-bench Evaluation ==="
echo "Results:      ${RESULTS}"
echo "Patches:      ${PATCHES_DIR}"
echo "Dataset:      ${DATASET}"
echo "Predictions:  ${PREDICTIONS_FILE}"
echo ""

# Step 1: Export patches to SWE-bench format
echo "--- Exporting patches ---"
python3 "${SCRIPT_DIR}/export_patches.py" \
    --results "$RESULTS" \
    --patches-dir "$PATCHES_DIR" \
    --out "$PREDICTIONS_FILE"

# Step 2: Extract condition from metadata sidecar for run_id
CONDITION="unknown"
META_FILE="${PREDICTIONS_FILE%.json}.meta.json"
if [[ -f "$META_FILE" ]]; then
    CONDITION=$(python3 -c "import json; print(json.load(open('${META_FILE}')).get('condition','unknown'))" 2>/dev/null || echo "unknown")
fi
RUN_ID="spelunk-${CONDITION}-${TIMESTAMP}"

echo ""
echo "--- Running Docker evaluation ---"
python3 -m swebench.harness.run_evaluation \
    --dataset_name "$DATASET" \
    --split "$SPLIT" \
    --predictions_path "$PREDICTIONS_FILE" \
    --max_workers "$MAX_WORKERS" \
    --timeout "$TIMEOUT" \
    --run_id "$RUN_ID"

# Step 3: Merge harness results back into result JSON
echo ""
echo "--- Merging resolve data ---"
# make_run_report() (swebench.harness.reporting) writes its summary report to
# "<model_name>.<run_id>.json" in the current working directory — NOT under
# swebench_eval_outputs/. Per-instance logs/test_output.txt live under
# logs/run_evaluation/${RUN_ID}/<model>/<instance>/, but the aggregate
# resolved/unresolved/error sets only exist in the top-level report file.
HARNESS_FILE=$(find . -maxdepth 1 -name "*.${RUN_ID}.json" 2>/dev/null | head -1)
if [[ -z "$HARNESS_FILE" ]]; then
    echo "ERROR: no harness report file matching '*.${RUN_ID}.json' found in $(pwd)" >&2
else
    echo "  Found: ${HARNESS_FILE}"
    python3 -c "
import json
raw = json.load(open('${RESULTS}'))
results = raw['tasks'] if isinstance(raw, dict) and 'tasks' in raw else raw
harness = json.load(open('${HARNESS_FILE}'))
resolved_map = {r: True for r in harness.get('resolved_ids', [])}

skipped_pre = sum(1 for r in results if r.get('skipped'))
errored = sum(1 for r in results if r.get('error'))
evaluated = 0
resolved_count = 0

for r in results:
    if not r.get('skipped') and not r.get('error'):
        evaluated += 1
        r['resolved'] = resolved_map.get(r.get('task_id', ''), False)
        if r['resolved']:
            resolved_count += 1

rate = resolved_count / evaluated if evaluated else 0
output = {
    'aggregate': {
        'tasks_total': len(results),
        'tasks_evaluated': evaluated,
        'tasks_resolved': resolved_count,
        'tasks_skipped_pre_eval': skipped_pre,
        'tasks_errored': errored,
        'resolve_rate': round(rate, 4),
    },
    'tasks': results,
}
json.dump(output, open('${RESULTS}', 'w'), indent=2)
print(f'  Evaluated: {evaluated}  Resolved: {resolved_count}  Rate: {rate:.1%}')
"
fi

echo ""
echo "=== Done ==="
echo "Per-instance logs in: logs/run_evaluation/${RUN_ID}/"
