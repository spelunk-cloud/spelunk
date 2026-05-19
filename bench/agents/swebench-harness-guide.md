# SWE-bench Harness — Setup & Run Guide

Step-by-step for running the full SWE-bench evaluation pipeline on any machine.

## Prerequisites

- **Disk space:** ≥50 GB free (Docker images for 24 tasks)
- **Docker:** installed and running (`docker ps` works)
- **git**, **uv**, **python3**, **spelunk** binary in PATH
- **DeepSeek API key** in `DEEPSEEK_API_KEY` env var or `.env.local`

## 1. Clone the repo

```bash
git clone https://github.com/usercise/spelunk.git
cd spelunk
git checkout rerun/benchmarks-20260517

# Build spelunk
cargo build --release
export SPELUNK="$(pwd)/target/release/spelunk"

# Create .env.local with API key
echo 'DEEPSEEK_API_KEY=sk-...' > .env.local
```

## 2. Clone SWE-bench repos (one-time, ~30 min)

```bash
# Clones 24 repos from GitHub into bench/repos/
bash bench/setup_repos.sh
```

If you already have repos checked out elsewhere, use `--repos-dir`:
```bash
bash bench/setup_repos.sh --repos-dir /path/to/existing/repos
```

Note: 26 of the 50 tasks in `tasks_50.json` are not in the Verified split.
The script will clone only those it finds (typically 24).

## 3. Generate agent patches (6 conditions × ~60 min, ~$25 API cost)

```bash
# Baseline (no spelunk)
uv run --with python-dotenv python3 bench/agents/batch_run.py \
    --condition baseline --tasks 50 --out bench/results/swebench-baseline.json

# With semantic search
uv run --with python-dotenv python3 bench/agents/batch_run.py \
    --condition spelunk_search --tasks 50 --out bench/results/swebench-spelunk_search.json

# With full spelunk (search + graph + memory)
uv run --with python-dotenv python3 bench/agents/batch_run.py \
    --condition spelunk_full --tasks 50 --out bench/results/swebench-spelunk_full.json
```

All three use `--save-patch` internally. Patches land in
`bench/patches/<condition>/<task_id>.patch`.

## 4. Build Docker images (one-time, ~2-4 hours, ~30 GB)

```bash
uv run --with swebench --with datasets --with docker python3 -c "
from swebench.harness.docker_build import build_instance_images
from datasets import load_dataset
import docker, json

with open('bench/agents/tasks_50.json') as f:
    task_ids = json.load(f)
ds = load_dataset('princeton-nlp/SWE-bench_Verified', split='test')
valid = [r for r in ds if r['instance_id'] in task_ids]
print(f'Building images for {len(valid)} tasks...')
build_instance_images(docker.from_env(), valid, max_workers=2,
                      tag='latest', env_image_tag='latest')
print('Done')
"
```

## 5. Run Docker evaluation (per condition, ~30 min each)

```bash
# Baseline
bash bench/agents/swebench_eval.sh \
    --results bench/results/swebench-baseline.json \
    --patches-dir bench/patches/baseline \
    --max-workers 4

# Spelunk search
bash bench/agents/swebench_eval.sh \
    --results bench/results/swebench-spelunk_search.json \
    --patches-dir bench/patches/spelunk_search \
    --max-workers 4

# Spelunk full
bash bench/agents/swebench_eval.sh \
    --results bench/results/swebench-spelunk_full.json \
    --patches-dir bench/patches/spelunk_full \
    --max-workers 4
```

Each run:
- Exports patches to SWE-bench format
- Runs Docker containers per task
- Merges `resolved` flags back into the result JSON
- Outputs aggregate `resolve_rate` with explicit denominator

## 6. Read results

```bash
python3 -c "
import json

for cond in ['baseline', 'spelunk_search', 'spelunk_full']:
    with open(f'bench/results/swebench-{cond}.json') as f:
        data = json.load(f)
    agg = data['aggregate']
    print(f'{cond}: resolve_rate={agg[\"resolve_rate\"]:.1%} '
          f'({agg[\"tasks_resolved\"]}/{agg[\"tasks_evaluated\"]}) '
          f'[skipped={agg[\"tasks_skipped_pre_eval\"]}]')
"
```

## Path flexibility

All scripts accept `--repos-dir` / `--repo-path` / `--api-key` / `--out` flags.
Nothing is hardcoded to `~/opensource/spelunk-bench/repos/` — the defaults
fall back to `bench/repos/` when the hardcoded path doesn't exist on the
current machine.
