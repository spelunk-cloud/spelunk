#!/usr/bin/env python3
"""Batch run agent.py across tasks, writing results incrementally.

Usage:
    python3 bench/agents/batch_run.py \\
        --condition spelunk_search \\
        --tasks 50 \\
        --batch-size 5 \\
        --out bench/results/swebench-spelunk_search.json
"""

import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parents[1]
TASKS_FILE = SCRIPT_DIR / "tasks_50.json"
AGENT_SCRIPT = SCRIPT_DIR / "agent.py"
_DEFAULT_REPOS = (
    Path.home() / "opensource" / "spelunk-bench" / "repos"
    if (Path.home() / "opensource" / "spelunk-bench" / "repos").is_dir()
    else SCRIPT_DIR.parent / "repos"
)


def load_task_ids(limit: int) -> list[str]:
    with open(TASKS_FILE) as f:
        tasks = json.load(f)
    if limit > 0:
        tasks = tasks[:limit]
    return tasks


def run_task(
    task_id: str,
    condition: str,
    model: str,
    api_base: str,
    api_key: str,
    max_turns: int,
    seed: int,
    repos_dir: Path,
) -> dict | None:
    repo_path = repos_dir / task_id
    issue_file = repo_path / "ISSUE.txt"

    if not repo_path.is_dir():
        return {"task_id": task_id, "skipped": True, "reason": "repo not found"}
    if not issue_file.is_file():
        return {"task_id": task_id, "skipped": True, "reason": "ISSUE.txt missing"}

    cmd = [
        "uv",
        "run",
        "--quiet",
        "--with-requirements",
        str(SCRIPT_DIR.parent / "requirements.txt"),
        "python3",
        str(AGENT_SCRIPT),
        "--condition",
        condition,
        "--task-id",
        task_id,
        "--repo-path",
        str(repo_path),
        "--issue",
        str(issue_file),
        "--model",
        model,
        "--api-base-url",
        api_base,
        "--api-key",
        api_key,
        "--max-turns",
        str(max_turns),
        "--seed",
        str(seed),
        "--save-patch",
        str(SCRIPT_DIR.parent / "patches" / condition / f"{task_id}.patch"),
    ]

    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
        if result.returncode == 0:
            # Parse the last JSON line from stdout
            for line in reversed(result.stdout.strip().splitlines()):
                line = line.strip()
                if line.startswith("{"):
                    return json.loads(line)
            return {
                "task_id": task_id,
                "error": True,
                "stderr": "no JSON in output",
                "stdout": result.stdout[-500:],
            }
        else:
            return {"task_id": task_id, "error": True, "stderr": result.stderr[:500]}
    except subprocess.TimeoutExpired:
        return {"task_id": task_id, "error": True, "stderr": "timeout after 600s"}
    except Exception as e:
        return {"task_id": task_id, "error": True, "stderr": str(e)}


def main():
    parser = argparse.ArgumentParser(description="Batch run SWE-bench agent.")
    parser.add_argument(
        "--condition",
        required=True,
        choices=["baseline", "spelunk_search", "spelunk_full"],
    )
    parser.add_argument("--tasks", type=int, default=50)
    parser.add_argument("--batch-size", type=int, default=5)
    parser.add_argument("--model", default="deepseek-v4-flash")
    parser.add_argument("--api-base-url", default="https://api.deepseek.com/v1")
    parser.add_argument("--api-key", default=None)
    parser.add_argument("--max-turns", type=int, default=20)
    parser.add_argument(
        "--repos-dir",
        default=str(_DEFAULT_REPOS),
        help="Directory containing per-task repo checkouts.",
    )
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    # Resolve API key
    api_key = args.api_key
    if not api_key:
        from dotenv import load_dotenv

        dotenv_path = REPO_ROOT / ".env.local"
        if dotenv_path.exists():
            load_dotenv(dotenv_path)
        import os

        api_key = os.environ.get("DEEPSEEK_API_KEY", "")
    if not api_key:
        parser.error("No API key. Use --api-key or set DEEPSEEK_API_KEY.")

    task_ids = load_task_ids(args.tasks)
    total = len(task_ids)
    all_results = []
    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)

    print(f"Condition: {args.condition}")
    print(f"Model:     {args.model}")
    print(f"Tasks:     {total}")
    print(f"Batch:     {args.batch_size}")
    print(f"Output:    {out_path}")
    print()

    for i, task_id in enumerate(task_ids):
        print(f"[{i + 1}/{total}] {task_id} ... ", end="", flush=True)
        start = time.monotonic()

        result = run_task(
            task_id,
            args.condition,
            args.model,
            args.api_base_url,
            api_key,
            args.max_turns,
            args.seed,
            Path(args.repos_dir),
        )

        elapsed = time.monotonic() - start
        if result is None:
            print(f"ERROR")
            result = {"task_id": task_id, "error": True}
        elif result.get("skipped"):
            print(f"SKIP ({result.get('reason', '')})")
        elif result.get("error"):
            print(f"ERROR ({result.get('stderr', '')[:80]})")
        else:
            turns = result.get("turns", "?")
            tokens = result.get("input_tokens", 0) + result.get("output_tokens", 0)
            print(f"OK  turns={turns} tokens={tokens:,} wall={elapsed:.1f}s")

        all_results.append(result)

        # Write incrementally after each batch
        if (i + 1) % args.batch_size == 0 or i == total - 1:
            with open(out_path, "w") as f:
                json.dump(all_results, f, indent=2)
            ran = sum(1 for r in all_results if "turns" in r)
            skip = sum(1 for r in all_results if r.get("skipped"))
            errs = sum(1 for r in all_results if r.get("error"))
            print(
                f"  -> saved {i + 1}/{total} results (ran={ran} skip={skip} err={errs})"
            )

        # Rate-limit
        time.sleep(1)

    # Final summary
    ran = [r for r in all_results if "turns" in r]
    print()
    print("=== Done ===")
    print(f"Total:  {len(all_results)}")
    print(f"Ran:    {len(ran)}")
    print(f"Skipped: {sum(1 for r in all_results if r.get('skipped'))}")
    print(f"Errors:  {sum(1 for r in all_results if r.get('error'))}")
    if ran:
        import statistics

        total_in = sum(r.get("input_tokens", 0) for r in ran)
        total_out = sum(r.get("output_tokens", 0) for r in ran)
        walls = [r.get("wall_seconds", 0) for r in ran]
        print(f"Input tokens:  {total_in:,}")
        print(f"Output tokens: {total_out:,}")
        print(f"Median wall:   {statistics.median(walls):.1f}s")


if __name__ == "__main__":
    main()
