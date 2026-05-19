#!/usr/bin/env python3
"""Code-graph benchmark — grep vs spelunk_search vs spelunk_graph.

For each task (a symbol in an indexed repo), runs three conditions and
measures how well each retrieves the ground-truth set of files containing
callers/callees/implementers of that symbol.

Conditions:
    grep           — git grep -w <symbol> over the repo
    spelunk_search — spelunk search <symbol> (semantic)
    spelunk_graph  — spelunk graph <symbol> --format json (flat edge list)

Metrics: precision@k, recall@k, F1. No LLM, no API costs.

Usage:
    python bench/graph/evaluate.py \\
        --tasks bench/graph/tasks.json \\
        --k 10 \\
        --out bench/results/graph.json

Task format (JSON):
    [
        {
            "symbol": "parse_args",
            "repo_path": "/path/to/indexed/repo",
            "ground_truth_files": ["src/cli.rs", "src/parser.rs"]
        }
    ]
"""

import argparse
import json
import statistics
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path


def run_grep(repo_path: Path, symbol: str, limit: int = 10) -> set[str]:
    """Return set of file paths containing the symbol via git grep -w, capped at limit."""
    try:
        result = subprocess.run(
            ["git", "grep", "-l", "-w", symbol],
            cwd=repo_path,
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode == 0:
            files = result.stdout.strip().split("\n")
            return set(files[:limit])
        return set()
    except Exception:
        return set()


def run_spelunk_search(repo_path: Path, symbol: str, limit: int = 10) -> set[str]:
    """Return set of file paths from spelunk search results (file_path field)."""
    try:
        result = subprocess.run(
            ["spelunk", "search", symbol, "--limit", str(limit), "--format", "json"],
            cwd=repo_path,
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode == 0 and result.stdout.strip():
            results = json.loads(result.stdout)
            return {r["file_path"] for r in results if r.get("file_path")}
        return set()
    except Exception:
        return set()


def run_spelunk_graph(repo_path: Path, symbol: str, limit: int = 10) -> set[str]:
    """Return set of file paths from spelunk graph results (flat edge list, source_file field)."""
    try:
        result = subprocess.run(
            ["spelunk", "graph", symbol, "--format", "json"],
            cwd=repo_path,
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode == 0 and result.stdout.strip():
            edges = json.loads(result.stdout)  # flat list of edge dicts
            files_ordered = []
            seen = set()
            for edge in edges:
                f = edge.get("source_file", "")
                if f and f not in seen:
                    seen.add(f)
                    files_ordered.append(f)
            return set(files_ordered[:limit])
        return set()
    except Exception:
        return set()


def precision(retrieved: set[str], relevant: set[str]) -> float:
    if not retrieved:
        return 0.0
    return len(retrieved & relevant) / len(retrieved)


def recall(retrieved: set[str], relevant: set[str]) -> float:
    if not relevant:
        return 1.0
    return len(retrieved & relevant) / len(relevant)


def f1(p: float, r: float) -> float:
    if p + r == 0:
        return 0.0
    return 2 * p * r / (p + r)


def get_spelunk_version() -> str:
    try:
        r = subprocess.run(
            ["spelunk", "--version"], capture_output=True, text=True, timeout=5
        )
        return r.stdout.strip()
    except Exception:
        return "unknown"


def main():
    parser = argparse.ArgumentParser(description="Code-graph benchmark.")
    parser.add_argument("--tasks", required=True, help="Tasks JSON file.")
    parser.add_argument("--k", type=int, default=10, help="Result limit (default: 10).")
    parser.add_argument("--out", default=None)
    args = parser.parse_args()

    with open(args.tasks) as f:
        tasks = json.load(f)

    print(f"Tasks: {len(tasks)}")
    print()

    conditions = {
        "grep": {"precisions": [], "recalls": [], "f1s": [], "wall": []},
        "spelunk_search": {"precisions": [], "recalls": [], "f1s": [], "wall": []},
        "spelunk_graph": {"precisions": [], "recalls": [], "f1s": [], "wall": []},
    }

    for i, task in enumerate(tasks):
        symbol = task["symbol"]
        repo_path = Path(task["repo_path"]).expanduser().resolve()
        relevant = set(task["ground_truth_files"])

        print(f"[{i + 1}/{len(tasks)}] {symbol} ({len(relevant)} ground-truth files)")

        for cond_name, runners in [
            ("grep", lambda: run_grep(repo_path, symbol, args.k)),
            ("spelunk_search", lambda: run_spelunk_search(repo_path, symbol, args.k)),
            ("spelunk_graph", lambda: run_spelunk_graph(repo_path, symbol, args.k)),
        ]:
            start = time.monotonic()
            retrieved = runners()
            elapsed = time.monotonic() - start

            p = precision(retrieved, relevant)
            r = recall(retrieved, relevant)
            f = f1(p, r)

            conditions[cond_name]["precisions"].append(p)
            conditions[cond_name]["recalls"].append(r)
            conditions[cond_name]["f1s"].append(f)
            conditions[cond_name]["wall"].append(elapsed)

            print(
                f"  {cond_name:<18} P={p:.2f}  R={r:.2f}  F1={f:.2f}  "
                f"retrieved={len(retrieved)}/{len(relevant)}  {elapsed:.2f}s"
            )

    output = {
        "benchmark": "code_graph",
        "spelunk_version": get_spelunk_version(),
        "k": args.k,
        "num_tasks": len(tasks),
        "timestamp": datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ"),
    }

    print()
    for cond_name, cond_data in conditions.items():
        ps = cond_data["precisions"]
        rs = cond_data["recalls"]
        fs = cond_data["f1s"]
        ws = cond_data["wall"]
        n = len(ps)
        if n:
            output[cond_name] = {
                "precision": round(statistics.mean(ps), 4),
                "recall": round(statistics.mean(rs), 4),
                "f1": round(statistics.mean(fs), 4),
                "median_wall_seconds": round(statistics.median(ws), 3),
            }
            print(
                f"{cond_name:<18} P={statistics.mean(ps):.3f}  R={statistics.mean(rs):.3f}  "
                f"F1={statistics.mean(fs):.3f}  wall={statistics.median(ws):.3f}s"
            )

    if args.out:
        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        with open(args.out, "w") as f:
            json.dump(output, f, indent=2)
        print(f"\nResults written to: {args.out}")
    else:
        print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
