#!/usr/bin/env bash
# Fetch SWE-bench task metadata and clone repos at the correct base commits.
#
# Each task directory under REPOS_DIR will contain the repo source tree at the
# pre-fix commit, plus an ISSUE.txt with the problem statement.
#
# Usage:
#   bash bench/setup_repos.sh [options]
#
# Options:
#   --tasks-file FILE   path to tasks JSON array  (default: bench/agents/tasks_50.json)
#   --tasks N           only set up first N tasks  (default: all)
#   --repos-dir DIR     checkout root              (default: bench/repos)
#   --dataset SLUG      HuggingFace dataset        (default: princeton-nlp/SWE-bench)
#                       NOTE: full split used (not SWE-bench_Verified) so all 50 tasks
#                       in tasks_50.json resolve — see issue #252.
#   --retries N         max retries per git op     (default: 3)
#   --git-timeout SEC   timeout per git command    (default: 120)
#   -h|--help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

TASKS_FILE="${SCRIPT_DIR}/../agents/tasks_50.json"
TASKS=0          # 0 = all

# Default to the shared spelunk-bench checkout if it exists
if [[ -d "${HOME}/opensource/spelunk-bench/repos" ]]; then
    REPOS_DIR="${HOME}/opensource/spelunk-bench/repos"
else
    REPOS_DIR="${SCRIPT_DIR}/repos"
fi
DATASET="princeton-nlp/SWE-bench"
MAX_RETRIES=3
GIT_TIMEOUT=120

usage() {
    grep '^#' "$0" | grep -v '#!/' | sed 's/^# \?//'
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tasks-file) TASKS_FILE="$2";  shift 2 ;;
        --tasks)      TASKS="$2";       shift 2 ;;
        --repos-dir)  REPOS_DIR="$2";   shift 2 ;;
        --dataset)    DATASET="$2";     shift 2 ;;
        --retries)    MAX_RETRIES="$2"; shift 2 ;;
        --git-timeout) GIT_TIMEOUT="$2"; shift 2 ;;
        -h|--help)    usage ;;
        *) echo "Unknown argument: $1" >&2; usage ;;
    esac
done

mkdir -p "$REPOS_DIR"

# ---------------------------------------------------------------------------
# Retry helper — retries a command with exponential backoff.
# Usage: retry <attempts> <description> <command...>
# ---------------------------------------------------------------------------
retry() {
    local attempts="$1"
    local desc="$2"
    shift 2

    local attempt=1
    local delay=5
    while [[ $attempt -le $attempts ]]; do
        if "$@" 2>/tmp/spelunk-setup-git-stderr.$$; then
            return 0
        fi
        local rc=$?
        if [[ $attempt -lt $attempts ]]; then
            echo "    Retry ${attempt}/${attempts}: ${desc} failed (exit ${rc}), retrying in ${delay}s..." >&2
            sleep "$delay"
            delay=$((delay * 2))
            # Cap at 60s
            [[ $delay -gt 60 ]] && delay=60
        fi
        attempt=$((attempt + 1))
    done
    return $rc
}

# ---------------------------------------------------------------------------
# Fetch metadata with retry
# ---------------------------------------------------------------------------
fetch_metadata() {
    local max_attempts=3
    local attempt=1
    local delay=5

    while [[ $attempt -le $max_attempts ]]; do
        local result
        result="$(uv run --with datasets --with huggingface_hub python3 - <<PYEOF 2>/tmp/spelunk-setup-hf-stderr.$$
import json, sys
from datasets import load_dataset

with open('${TASKS_FILE}') as f:
    task_ids = json.load(f)

limit = int('${TASKS}')
if limit > 0:
    task_ids = task_ids[:limit]

task_set = set(task_ids)

ds = load_dataset('${DATASET}', split='test')
for row in ds:
    if row['instance_id'] in task_set:
        print(json.dumps({
            'instance_id':       row['instance_id'],
            'repo':              row['repo'],
            'base_commit':       row['base_commit'],
            'problem_statement': row['problem_statement'],
        }))
PYEOF
)"
        local rc=$?
        if [[ $rc -eq 0 ]]; then
            echo "$result"
            return 0
        fi
        if [[ $attempt -lt $max_attempts ]]; then
            echo "  WARNING: dataset fetch failed (attempt ${attempt}/${max_attempts}), retrying in ${delay}s..." >&2
            echo "  stderr: $(head -5 /tmp/spelunk-setup-hf-stderr.$$ 2>/dev/null || true)" >&2
            sleep "$delay"
            delay=$((delay * 2))
            [[ $delay -gt 60 ]] && delay=60
        fi
        attempt=$((attempt + 1))
    done
    echo ""  # empty = failure
    return 1
}

echo "Tasks file:  ${TASKS_FILE}"
echo "Repos dir:   ${REPOS_DIR}"
echo "Dataset:     ${DATASET}"
echo "Retries:     ${MAX_RETRIES}"
echo "Git timeout: ${GIT_TIMEOUT}s"
echo ""

# ---------------------------------------------------------------------------
# Fetch metadata
# ---------------------------------------------------------------------------
echo "Fetching task metadata from HuggingFace..."
METADATA_JSONL="$(fetch_metadata)"
if [[ -z "$METADATA_JSONL" ]]; then
    echo "ERROR: Failed to fetch dataset metadata after retries." >&2
    exit 1
fi

TOTAL="$(echo "$METADATA_JSONL" | grep -c . || true)"
echo "Fetched metadata for ${TOTAL} tasks."
echo ""

# ---------------------------------------------------------------------------
# Clone / update each repo
# ---------------------------------------------------------------------------
IDX=0
SUCCESS=0
SKIPPED=0
FAILED_TASKS=()

while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    IDX=$((IDX + 1))

    INSTANCE_ID="$(echo "$line" | python3 -c "import json,sys; print(json.load(sys.stdin)['instance_id'])")"
    REPO="$(echo        "$line" | python3 -c "import json,sys; print(json.load(sys.stdin)['repo'])")"
    BASE_COMMIT="$(echo "$line" | python3 -c "import json,sys; print(json.load(sys.stdin)['base_commit'])")"
    PROBLEM="$(echo     "$line" | python3 -c "import json,sys; print(json.load(sys.stdin)['problem_statement'])")"

    DEST="${REPOS_DIR}/${INSTANCE_ID}"
    echo "[${IDX}/${TOTAL}] ${INSTANCE_ID} (${REPO} @ ${BASE_COMMIT:0:12})"

    # --- Check if already at the correct commit ---
    if [[ -f "${DEST}/ISSUE.txt" ]] && git -C "$DEST" rev-parse --verify HEAD &>/dev/null; then
        CURRENT="$(git -C "$DEST" rev-parse HEAD)"
        if [[ "$CURRENT" == "$BASE_COMMIT" ]]; then
            echo "  Already set up — skipping."
            SKIPPED=$((SKIPPED + 1))
            continue
        else
            echo "  Repo exists at wrong commit (${CURRENT:0:12}), re-fetching..."
        fi
    fi

    # --- Clone or update the repo ---
    CLONE_URL="https://github.com/${REPO}.git"
    CLONE_OK=true

    if [[ -d "$DEST/.git" ]]; then
        # Existing repo: fetch with timeout and retry
        echo "  Fetching origin..."
        if ! retry "$MAX_RETRIES" "git fetch" \
            timeout "$GIT_TIMEOUT" git -C "$DEST" fetch --quiet origin; then
            echo "  ERROR: git fetch failed after ${MAX_RETRIES} retries."
            FAILED_TASKS+=("${INSTANCE_ID}: git fetch failed")
            continue
        fi
    else
        # New clone — try partial (blobless) clone first, fall back to full clone
        echo "  Cloning ${CLONE_URL}..."
        if retry "$MAX_RETRIES" "git clone (blobless)" \
            timeout "$GIT_TIMEOUT" git clone --filter=blob:none --no-checkout --quiet "$CLONE_URL" "$DEST" 2>/dev/null; then
            echo "    (partial clone OK)"
        elif retry "$MAX_RETRIES" "git clone (full)" \
            timeout "$GIT_TIMEOUT" git clone --no-checkout --quiet "$CLONE_URL" "$DEST" 2>/dev/null; then
            echo "    (full clone OK — partial clone not supported by server)"
        else
            echo "  ERROR: git clone failed after retries."
            FAILED_TASKS+=("${INSTANCE_ID}: git clone failed")
            # Clean up partial clone dir so it doesn't look like a repo
            rm -rf "$DEST" 2>/dev/null || true
            continue
        fi
    fi

    # --- Checkout target commit ---
    echo "  Checking out ${BASE_COMMIT:0:12}..."
    if ! retry "$MAX_RETRIES" "git checkout" \
        git -C "$DEST" checkout --quiet "$BASE_COMMIT"; then
        echo "  ERROR: git checkout failed (commit may not exist on remote)."
        FAILED_TASKS+=("${INSTANCE_ID}: git checkout ${BASE_COMMIT:0:12} failed")
        continue
    fi

    # --- Verify checkout ---
    VERIFIED="$(git -C "$DEST" rev-parse HEAD 2>/dev/null || echo "")"
    if [[ "$VERIFIED" != "$BASE_COMMIT" ]]; then
        echo "  ERROR: checkout verification failed. Expected ${BASE_COMMIT:0:12}, got ${VERIFIED:0:12}."
        FAILED_TASKS+=("${INSTANCE_ID}: checkout verification failed")
        continue
    fi

    # --- Write ISSUE.txt ---
    printf '%s\n' "$PROBLEM" > "${DEST}/ISSUE.txt"
    echo "  Done: checked out ${BASE_COMMIT:0:12}"
    SUCCESS=$((SUCCESS + 1))

done <<< "$METADATA_JSONL"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=============================================="
echo "Setup complete."
echo "  Success: ${SUCCESS}"
echo "  Skipped (already current): ${SKIPPED}"
echo "  Failed:  ${#FAILED_TASKS[@]}"

if [[ ${#FAILED_TASKS[@]} -gt 0 ]]; then
    echo ""
    echo "Failed tasks:"
    for ft in "${FAILED_TASKS[@]}"; do
        echo "  - ${ft}"
    done
    echo ""
    echo "Re-run to retry:"
    echo "  bash bench/setup_repos.sh --retries 5 --git-timeout 300"
fi
