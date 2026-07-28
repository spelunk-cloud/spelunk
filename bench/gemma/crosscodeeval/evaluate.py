#!/usr/bin/env python3
"""Evaluate DeepSeek on RepoBench-Python cross-file completion.

Four conditions for controlled comparison:

    baseline_single_shot — one API call, no tools, no loop
    multi_turn_no_tools — 5-turn loop, no tools (isolates loop from retrieval)
    naive_search        — 5-turn loop with read_file + run_grep tools
    spelunk             — 5-turn loop with spelunk_search tool

Usage:
    python bench/gemma/crosscodeeval/evaluate.py --condition baseline_single_shot
    python bench/gemma/crosscodeeval/evaluate.py --condition multi_turn_no_tools
    python bench/gemma/crosscodeeval/evaluate.py --condition naive_search --repo-path /path/to/repo
    python bench/gemma/crosscodeeval/evaluate.py --condition spelunk --repo-path /path/to/repo
"""

import argparse
import difflib
import json
import os
import re
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import numpy as np
from datasets import load_dataset
from dotenv import load_dotenv
from openai import OpenAI
from tqdm import tqdm

# Auto-load .env.local from project root if present
_load_root = Path(__file__).resolve().parents[3]
_dotenv_path = _load_root / ".env.local"
if _dotenv_path.exists():
    load_dotenv(_dotenv_path)

MAX_SEARCH_TURNS = 5
MAX_OUTPUT_CHARS = 4_000
VALID_CONDITIONS = (
    "baseline_single_shot",
    "multi_turn_no_tools",
    "naive_search",
    "spelunk",
)

# ---------------------------------------------------------------------------
# System prompts
# ---------------------------------------------------------------------------

SYSTEM_BASELINE = (
    "You are a code completion assistant. "
    "Complete the next line of the given code. "
    "Output only the completion line, nothing else — no explanation, no markdown fences."
)

SYSTEM_MULTI_TURN = (
    "You are a code completion assistant. You have several turns to think about "
    "the code before producing the final answer. On each turn you may analyse the "
    "code and reason about what the next line should be. When you are ready, "
    "output only the completion line — nothing else, no explanation, no markdown fences."
)

SYSTEM_NAIVE = (
    "You are a code completion assistant. You can use read_file to inspect other "
    "files in the codebase and run_grep to search for symbols before completing "
    "the code. Output only the completion line — nothing else, no explanation, "
    "no markdown fences, no code fences."
)

SYSTEM_SPELUNK = (
    "You are a code completion assistant. "
    "Use spelunk_search to find relevant type definitions, function signatures, or constants "
    "from other files in the codebase before completing the code. "
    "Output only the completion line, nothing else — no explanation, no markdown fences."
)

# ---------------------------------------------------------------------------
# Tool definitions (OpenAI function-calling format)
# ---------------------------------------------------------------------------

SPELUNK_TOOL = {
    "type": "function",
    "function": {
        "name": "spelunk_search",
        "description": (
            "Semantically search the indexed codebase for relevant code — type definitions, "
            "function signatures, constants, or class structures from other files."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language search query.",
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (default 5).",
                    "default": 5,
                },
            },
            "required": ["query"],
        },
    },
}

READ_FILE_TOOL = {
    "type": "function",
    "function": {
        "name": "read_file",
        "description": "Read the contents of a file in the repository. Use this to inspect cross-file dependencies.",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path relative to the repo root.",
                },
            },
            "required": ["path"],
        },
    },
}

RUN_GREP_TOOL = {
    "type": "function",
    "function": {
        "name": "run_grep",
        "description": "Search for a pattern in the repository using grep. Use this to find where a symbol is defined or used.",
        "parameters": {
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex or literal pattern to search for.",
                },
                "path": {
                    "type": "string",
                    "description": "Subdirectory to search (default: repo root).",
                    "default": ".",
                },
            },
            "required": ["pattern"],
        },
    },
}

# ---------------------------------------------------------------------------
# Tool implementations
# ---------------------------------------------------------------------------


def _read_file(repo_path: Path, path: str) -> str:
    target = (repo_path / path).resolve()
    if not str(target).startswith(str(repo_path.resolve())):
        return "Error: path is outside the repository."
    try:
        content = target.read_text(errors="replace")
        if len(content) > MAX_OUTPUT_CHARS:
            content = content[:MAX_OUTPUT_CHARS] + "\n... (truncated)"
        return content
    except Exception as e:
        return f"Error reading file: {e}"


def _run_grep(repo_path: Path, pattern: str, search_path: str = ".") -> str:
    target = (repo_path / search_path).resolve()
    if not str(target).startswith(str(repo_path.resolve())):
        target = repo_path
    try:
        result = subprocess.run(
            ["grep", "-rn", "--include=*.py", pattern, str(target)],
            capture_output=True,
            text=True,
            timeout=15,
        )
        output = result.stdout + result.stderr
        if not output.strip():
            return "(no matches)"
        if len(output) > MAX_OUTPUT_CHARS:
            output = output[:MAX_OUTPUT_CHARS] + "\n... (truncated)"
        return output
    except Exception as e:
        return f"Error running grep: {e}"


def _spelunk_search(repo_path: Path, query: str, limit: int = 5) -> str:
    cmd = ["spelunk", "search", query, "--limit", str(limit), "--format", "json"]
    try:
        result = subprocess.run(
            cmd, cwd=repo_path, capture_output=True, text=True, timeout=30
        )
        output = result.stdout
        if result.returncode != 0:
            return f"spelunk search failed: {result.stderr.strip()}"
    except FileNotFoundError:
        return "Error: spelunk not found in PATH."
    except subprocess.TimeoutExpired:
        return "Error: spelunk search timed out."
    if len(output) > MAX_OUTPUT_CHARS:
        output = output[:MAX_OUTPUT_CHARS] + "\n... (truncated)"
    return output or "(no results)"


def _append_assistant(msg, messages: list) -> None:
    """Append assistant message, preserving reasoning_content for DeepSeek."""
    entry: dict = {"role": "assistant", "content": msg.content or ""}
    if hasattr(msg, "reasoning_content") and msg.reasoning_content:
        entry["reasoning_content"] = msg.reasoning_content
    if msg.tool_calls:
        entry["tool_calls"] = [
            {
                "id": tc.id,
                "type": "function",
                "function": {
                    "name": tc.function.name,
                    "arguments": tc.function.arguments,
                },
            }
            for tc in msg.tool_calls
        ]
    messages.append(entry)


def _dispatch_naive(repo_path: Path, name: str, args: dict) -> str:
    if name == "read_file":
        return _read_file(repo_path, args["path"])
    elif name == "run_grep":
        return _run_grep(repo_path, args["pattern"], args.get("path", "."))
    return f"Unknown tool: {name}"


def _dispatch_spelunk(repo_path: Path, name: str, args: dict) -> str:
    if name == "spelunk_search":
        return _spelunk_search(repo_path, args["query"], args.get("limit", 5))
    return f"Unknown tool: {name}"


# ---------------------------------------------------------------------------
# Completion functions — each returns (prediction, input_tokens, output_tokens)
# ---------------------------------------------------------------------------


def complete_baseline_single_shot(
    client: OpenAI, model: str, prompt: str, seed: int
) -> tuple[str, int, int]:
    response = client.chat.completions.create(
        model=model,
        max_tokens=256,
        temperature=0.0,
        seed=seed,
        messages=[
            {"role": "system", "content": SYSTEM_BASELINE},
            {"role": "user", "content": f"Complete the next line:\n\n{prompt}"},
        ],
    )
    content = response.choices[0].message.content or ""
    return (
        content.strip(),
        response.usage.prompt_tokens,
        response.usage.completion_tokens,
    )


def complete_multi_turn_no_tools(
    client: OpenAI, model: str, prompt: str, seed: int
) -> tuple[str, int, int]:
    """5-turn loop with no tools — same compute budget as spelunk, no retrieval."""
    messages = [
        {"role": "system", "content": SYSTEM_MULTI_TURN},
        {"role": "user", "content": f"Complete the next line:\n\n{prompt}"},
    ]
    input_tokens = 0
    output_tokens = 0

    for turn in range(MAX_SEARCH_TURNS):
        response = client.chat.completions.create(
            model=model,
            max_tokens=512,
            temperature=0.0,
            seed=seed,
            messages=messages,
        )
        msg = response.choices[0].message
        input_tokens += response.usage.prompt_tokens
        output_tokens += response.usage.completion_tokens

        _append_assistant(msg, messages)

        if response.choices[0].finish_reason == "stop":
            return (msg.content or "").strip(), input_tokens, output_tokens

        # Re-prompt to keep the model thinking
        if turn < MAX_SEARCH_TURNS - 1:
            messages.append(
                {
                    "role": "user",
                    "content": "Continue analysing. When ready, output only the completion line.",
                }
            )

    # Fell through — ask for final answer
    messages.append({"role": "user", "content": "Now output only the completion line."})
    response = client.chat.completions.create(
        model=model,
        max_tokens=256,
        temperature=0.0,
        seed=seed,
        messages=messages,
    )
    input_tokens += response.usage.prompt_tokens
    output_tokens += response.usage.completion_tokens
    return (
        (response.choices[0].message.content or "").strip(),
        input_tokens,
        output_tokens,
    )


def complete_naive_search(
    client: OpenAI, model: str, prompt: str, repo_path: Path, seed: int
) -> tuple[str, int, int]:
    """5-turn loop with read_file + run_grep tools."""
    tools = [READ_FILE_TOOL, RUN_GREP_TOOL]
    messages = [
        {"role": "system", "content": SYSTEM_NAIVE},
        {"role": "user", "content": f"Complete the next line:\n\n{prompt}"},
    ]
    input_tokens = 0
    output_tokens = 0

    for _ in range(MAX_SEARCH_TURNS):
        response = client.chat.completions.create(
            model=model,
            max_tokens=512,
            temperature=0.0,
            seed=seed,
            tools=tools,
            tool_choice="auto",
            messages=messages,
        )
        msg = response.choices[0].message
        input_tokens += response.usage.prompt_tokens
        output_tokens += response.usage.completion_tokens

        _append_assistant(msg, messages)

        if response.choices[0].finish_reason != "tool_calls" or not msg.tool_calls:
            return (msg.content or "").strip(), input_tokens, output_tokens

        for tc in msg.tool_calls:
            args = json.loads(tc.function.arguments)
            result = _dispatch_naive(repo_path, tc.function.name, args)
            messages.append({"role": "tool", "tool_call_id": tc.id, "content": result})

    messages.append({"role": "user", "content": "Now output only the completion line."})
    response = client.chat.completions.create(
        model=model,
        max_tokens=256,
        temperature=0.0,
        seed=seed,
        messages=messages,
    )
    input_tokens += response.usage.prompt_tokens
    output_tokens += response.usage.completion_tokens
    return (
        (response.choices[0].message.content or "").strip(),
        input_tokens,
        output_tokens,
    )


def complete_spelunk(
    client: OpenAI, model: str, prompt: str, repo_path: Path, seed: int
) -> tuple[str, int, int]:
    """5-turn loop with spelunk_search tool."""
    messages = [
        {"role": "system", "content": SYSTEM_SPELUNK},
        {"role": "user", "content": f"Complete the next line:\n\n{prompt}"},
    ]
    input_tokens = 0
    output_tokens = 0

    for _ in range(MAX_SEARCH_TURNS):
        response = client.chat.completions.create(
            model=model,
            max_tokens=512,
            temperature=0.0,
            seed=seed,
            tools=[SPELUNK_TOOL],
            tool_choice="auto",
            messages=messages,
        )
        msg = response.choices[0].message
        input_tokens += response.usage.prompt_tokens
        output_tokens += response.usage.completion_tokens

        _append_assistant(msg, messages)

        if response.choices[0].finish_reason != "tool_calls" or not msg.tool_calls:
            return (msg.content or "").strip(), input_tokens, output_tokens

        for tc in msg.tool_calls:
            args = json.loads(tc.function.arguments)
            result = _dispatch_spelunk(repo_path, tc.function.name, args)
            messages.append({"role": "tool", "tool_call_id": tc.id, "content": result})

    messages.append({"role": "user", "content": "Now output only the completion line."})
    response = client.chat.completions.create(
        model=model,
        max_tokens=256,
        temperature=0.0,
        seed=seed,
        messages=messages,
    )
    input_tokens += response.usage.prompt_tokens
    output_tokens += response.usage.completion_tokens
    return (
        (response.choices[0].message.content or "").strip(),
        input_tokens,
        output_tokens,
    )


# ---------------------------------------------------------------------------
# Metrics
# ---------------------------------------------------------------------------


def edit_similarity(pred: str, truth: str) -> float:
    return difflib.SequenceMatcher(None, pred, truth).ratio()


def extract_identifiers(code: str) -> set[str]:
    return {t for t in re.findall(r"\b[A-Za-z_]\w+\b", code)}


def identifier_recall(pred: str, truth: str) -> float:
    truth_ids = extract_identifiers(truth)
    if not truth_ids:
        return 1.0
    pred_ids = extract_identifiers(pred)
    return len(truth_ids & pred_ids) / len(truth_ids)


def get_spelunk_version() -> str:
    try:
        r = subprocess.run(
            ["spelunk", "--version"], capture_output=True, text=True, timeout=10
        )
        return r.stdout.strip()
    except Exception:
        return "unknown"


def scaffold_hash() -> str:
    bench_dir = Path(__file__).parents[3]
    try:
        r = subprocess.run(
            ["git", "log", "-1", "--format=%H", "--", "bench/"],
            cwd=bench_dir,
            capture_output=True,
            text=True,
            timeout=10,
        )
        return r.stdout.strip() or "unknown"
    except Exception:
        return "unknown"


def detect_repo_owner_name(repo_path: Path) -> str | None:
    """Return 'owner/name' from the git remote origin URL, or None."""
    try:
        url = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            cwd=repo_path,
            capture_output=True,
            text=True,
            timeout=5,
        ).stdout.strip()
        m = re.search(r"[:/]([^/:]+)/([^/]+?)(?:\.git)?$", url)
        return f"{m.group(1)}/{m.group(2)}" if m else None
    except Exception:
        return None


# ---------------------------------------------------------------------------
# Dataset + evaluation
# ---------------------------------------------------------------------------

DATASET = "tianyang/repobench_python_v1.1"
VALID_SPLITS = ("cross_file_first", "cross_file_random", "in_file")


def evaluate_split(
    split: str,
    samples: int,
    condition: str,
    client: OpenAI,
    model: str,
    repo_path: Path | None,
    seed: int,
    repo_filter: str | None = None,
) -> tuple[list, list, list, list, list, list, float]:
    print(f"Loading RepoBench-Python ({split})...")
    dataset = load_dataset(DATASET, split=split)

    if repo_filter:
        dataset = dataset.filter(lambda x: x["repo_name"] == repo_filter)
        print(f"  Filtered to repo '{repo_filter}': {len(dataset)} tasks")
        if len(dataset) == 0:
            raise ValueError(f"No tasks found for repo '{repo_filter}'")

    n = min(samples, len(dataset))
    indices = np.random.choice(len(dataset), size=n, replace=False).tolist()
    sampled = dataset.select(indices)

    exact_matches, edit_sims, id_recalls = [], [], []
    input_tokens_list, output_tokens_list, wall_times = [], [], []

    for item in tqdm(sampled, desc=f"{split} ({condition})", unit="task"):
        prompt = item.get("cropped_code", "")
        truth = (item.get("next_line") or "").strip()

        if not prompt or not truth:
            continue

        start = time.monotonic()
        try:
            if condition == "baseline_single_shot":
                pred, inp_tok, out_tok = complete_baseline_single_shot(
                    client, model, prompt, seed
                )
            elif condition == "multi_turn_no_tools":
                pred, inp_tok, out_tok = complete_multi_turn_no_tools(
                    client, model, prompt, seed
                )
            elif condition == "naive_search":
                pred, inp_tok, out_tok = complete_naive_search(
                    client, model, prompt, repo_path, seed
                )
            elif condition == "spelunk":
                pred, inp_tok, out_tok = complete_spelunk(
                    client, model, prompt, repo_path, seed
                )
            else:
                raise ValueError(f"Unknown condition: {condition}")
        except Exception as e:
            print(f"\nWarning: inference failed — {e}")
            continue
        elapsed = time.monotonic() - start

        exact_matches.append(1.0 if pred == truth else 0.0)
        edit_sims.append(edit_similarity(pred, truth))
        id_recalls.append(identifier_recall(pred, truth))
        input_tokens_list.append(inp_tok)
        output_tokens_list.append(out_tok)
        wall_times.append(elapsed)

    indexed_name = detect_repo_owner_name(repo_path) if repo_path else None
    if indexed_name:
        hits = sum(1 for s in sampled if s.get("repo_name") == indexed_name)
        overlap_pct = (hits / len(sampled)) * 100 if sampled else 0.0
    else:
        overlap_pct = 0.0
    return (
        exact_matches,
        edit_sims,
        id_recalls,
        input_tokens_list,
        output_tokens_list,
        wall_times,
        overlap_pct,
    )


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Evaluate on RepoBench-Python cross-file completion."
    )
    parser.add_argument("--condition", choices=list(VALID_CONDITIONS), required=True)
    parser.add_argument(
        "--repo-path",
        default=None,
        help="Path to an indexed repo (required for naive_search and spelunk).",
    )
    parser.add_argument(
        "--split",
        default="cross_file_first",
        help=f"Dataset split: {', '.join(VALID_SPLITS)} (default: cross_file_first).",
    )
    parser.add_argument("--samples", type=int, default=100)
    parser.add_argument("--model", default="deepseek-v4-flash")
    parser.add_argument("--api-base-url", default="https://api.deepseek.com/v1")
    parser.add_argument(
        "--api-key",
        default=None,
        help="API key (falls back to DEEPSEEK_API_KEY env var).",
    )
    parser.add_argument(
        "--scaffold-hash",
        default=None,
        help="Passed by run.sh; auto-computed if omitted.",
    )
    parser.add_argument(
        "--repo-filter",
        default=None,
        help="Only evaluate tasks from this GitHub repo (owner/name).",
    )
    parser.add_argument("--out", default=None)
    parser.add_argument("--seed", type=int, default=42)
    args = parser.parse_args()

    if args.split not in VALID_SPLITS:
        parser.error(f"--split must be one of: {', '.join(VALID_SPLITS)}")
    if args.condition in ("naive_search", "spelunk") and not args.repo_path:
        parser.error(f"--repo-path is required for --condition {args.condition}")

    repo_path = Path(args.repo_path).resolve() if args.repo_path else None
    if repo_path and not repo_path.is_dir():
        parser.error(f"--repo-path does not exist: {repo_path}")

    # Cross-validate --repo-filter against --repo-path git remote
    if args.repo_filter and repo_path:
        indexed_name = detect_repo_owner_name(repo_path)
        if indexed_name and args.repo_filter != indexed_name:
            print(
                f"Warning: --repo-filter ({args.repo_filter}) does not match "
                f"git remote of --repo-path ({indexed_name}). "
                f"The benchmark will search a different repo than it samples from.",
                file=sys.stderr,
            )

    api_key = args.api_key or os.environ.get("DEEPSEEK_API_KEY", "local")
    provenance_label = (
        "flag:--api-key"
        if args.api_key
        else (
            "env:DEEPSEEK_API_KEY"
            if os.environ.get("DEEPSEEK_API_KEY")
            else "default:local"
        )
    )

    np.random.seed(args.seed)
    client = OpenAI(base_url=args.api_base_url, api_key=api_key)

    exact, edit, id_rec, inp_tok, out_tok, wall, overlap_pct = evaluate_split(
        args.split,
        args.samples,
        args.condition,
        client,
        args.model,
        repo_path,
        args.seed,
        repo_filter=args.repo_filter,
    )

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = {
        "run_id": timestamp,
        "benchmark": "repobench",
        "condition": args.condition,
        "model": args.model,
        "model_source": "api",
        "api_base_url": args.api_base_url,
        "api_key_source": provenance_label,
        "spelunk_version": get_spelunk_version(),
        "scaffold_hash": args.scaffold_hash or scaffold_hash(),
        "repo_filter": args.repo_filter,
        "indexed_repo_overlap_pct": round(overlap_pct, 1),
        "seed": args.seed,
        "split": args.split,
        "samples": len(exact),
        "exact_match": round(float(np.mean(exact)), 4) if exact else 0.0,
        "edit_similarity": round(float(np.mean(edit)), 4) if edit else 0.0,
        "identifier_recall": round(float(np.mean(id_rec)), 4) if id_rec else 0.0,
        "median_input_tokens": round(float(np.median(inp_tok)), 1) if inp_tok else 0.0,
        "median_output_tokens": round(float(np.median(out_tok)), 1) if out_tok else 0.0,
        "median_wall_seconds": round(float(np.median(wall)), 3) if wall else 0.0,
    }

    if args.out:
        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        Path(args.out).write_text(json.dumps(output, indent=2))
        print(f"Results written to: {args.out}")
    else:
        print(json.dumps(output, indent=2))

    print(f"\nExact match:       {output['exact_match']:.4f}")
    print(f"Edit similarity:   {output['edit_similarity']:.4f}")
    print(f"Identifier recall: {output['identifier_recall']:.4f}")
    print(f"Median wall:       {output['median_wall_seconds']:.3f}s")


if __name__ == "__main__":
    main()
