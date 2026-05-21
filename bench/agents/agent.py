#!/usr/bin/env python3
"""Unified SWE-bench agent — any OpenAI-compatible API, with or without spelunk.

Replaces bench/swebench/agent_*.py (Anthropic, deleted) and
bench/gemma/swebench_local/agent_*.py (local Gemma, kept as templates).

Three conditions, specified via --condition:

    baseline         read_file, run_bash, write_file
    spelunk_search   baseline + spelunk_search (semantic code retrieval)
    spelunk_full     baseline + spelunk_search + spelunk_graph + spelunk_memory_search

Usage:
    python bench/agents/agent.py \\
        --condition spelunk_full \\
        --task-id django__django-11099 \\
        --repo-path /path/to/repo \\
        --issue "Issue description..." \\
        --model deepseek-v4-flash \\
        --api-base-url https://api.deepseek.com/v1 \\
        --api-key $DEEPSEEK_API_KEY \\
        [--max-turns 20] [--seed 42]

Output: single JSON object on stdout (reproducibility contract fields).
"""

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

from dotenv import load_dotenv
from openai import OpenAI

# Auto-load .env.local from project root if present
_load_root = Path(__file__).resolve().parents[2]
_dotenv_path = _load_root / ".env.local"
if _dotenv_path.exists():
    load_dotenv(_dotenv_path)

# ---------------------------------------------------------------------------
# Tool definitions (OpenAI function-calling format)
# ---------------------------------------------------------------------------

BASE_TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read the contents of a file within the repository.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the repo root.",
                    }
                },
                "required": ["path"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "run_bash",
            "description": (
                "Run a shell command in the repository directory. "
                "Output is truncated to 10 000 characters."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute.",
                    }
                },
                "required": ["command"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "write_file",
            "description": "Write content to a file within the repository.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the repo root.",
                    },
                    "content": {
                        "type": "string",
                        "description": "Full content to write.",
                    },
                },
                "required": ["path", "content"],
            },
        },
    },
]

SPELUNK_SEARCH_TOOL = {
    "type": "function",
    "function": {
        "name": "spelunk_search",
        "description": (
            "Semantically search the codebase using spelunk. Returns the most "
            "relevant code chunks for the given query. Use this to quickly locate "
            "relevant functions, classes, or logic without manually browsing files."
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
                    "description": "Max results (default 10).",
                    "default": 10,
                },
            },
            "required": ["query"],
        },
    },
}

SPELUNK_GRAPH_TOOL = {
    "type": "function",
    "function": {
        "name": "spelunk_graph",
        "description": (
            "Query the spelunk code graph for a given symbol (function, struct, "
            "class, etc.). Returns callers, callees, and import relationships. "
            "Use this to trace how a symbol is used across the codebase."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": (
                        "Symbol name to query, e.g. a function or class name."
                    ),
                }
            },
            "required": ["symbol"],
        },
    },
}

SPELUNK_MEMORY_SEARCH_TOOL = {
    "type": "function",
    "function": {
        "name": "spelunk_memory_search",
        "description": (
            "Search spelunk project memory for decisions, notes, handoffs, and "
            "other contextual information. Use this to find prior design decisions, "
            "architectural context, or notes left by previous sessions."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language search query against project memory.",
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (default 10).",
                    "default": 10,
                },
            },
            "required": ["query"],
        },
    },
}

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

MAX_OUTPUT_CHARS = 10_000

SYSTEM_PROMPT_BASE = (
    "You are an expert software engineer. You are given a GitHub issue and a "
    "repository checkout. Your goal is to produce a minimal patch that fixes the "
    "issue. Use the available tools to explore the codebase, understand the problem, "
    "and apply the fix. When you are done, briefly summarise what you changed."
)

SYSTEM_PROMPT_SPELUNK = (
    "You are an expert software engineer. You are given a GitHub issue and a "
    "repository checkout. Your goal is to produce a minimal patch that fixes the "
    "issue. You have access to spelunk tools for fast semantic code search, "
    "code graph traversal, and project memory retrieval — use them to locate "
    "relevant code and context before diving into files. When you are done, "
    "briefly summarise what you changed."
)

# ---------------------------------------------------------------------------
# Tool dispatch
# ---------------------------------------------------------------------------


def read_file(repo_path: Path, path: str) -> str:
    target = (repo_path / path).resolve()
    repo_root = repo_path.resolve()
    if not str(target).startswith(str(repo_root)):
        return "Error: path is outside the repository."
    try:
        return target.read_text(errors="replace")
    except Exception as e:
        return f"Error reading file: {e}"


def run_bash(repo_path: Path, command: str, timeout: int = 60) -> str:
    try:
        result = subprocess.run(
            command,
            shell=True,
            cwd=repo_path,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        output = result.stdout + result.stderr
    except subprocess.TimeoutExpired:
        output = f"Error: command timed out after {timeout} seconds."
    except Exception as e:
        output = f"Error running command: {e}"
    if len(output) > MAX_OUTPUT_CHARS:
        output = output[:MAX_OUTPUT_CHARS] + "\n... (output truncated)"
    return output


def write_file(repo_path: Path, path: str, content: str) -> str:
    target = (repo_path / path).resolve()
    repo_root = repo_path.resolve()
    if not str(target).startswith(str(repo_root)):
        return "Error: path is outside the repository."
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)
        return f"Wrote {len(content)} bytes to {path}."
    except Exception as e:
        return f"Error writing file: {e}"


def _run_spelunk(repo_path: Path, args: list[str], timeout: int = 30) -> str:
    """Run a spelunk command in repo_path, return stdout or error message."""
    cmd = ["spelunk"] + args
    try:
        result = subprocess.run(
            cmd, cwd=repo_path, capture_output=True, text=True, timeout=timeout
        )
        if result.returncode == 0:
            output = result.stdout
        elif result.returncode == 1:
            # plumbing convention: exit 1 = no results — not an error
            output = result.stdout or "(no results)"
        else:
            return (
                f"spelunk {' '.join(args)} failed (exit {result.returncode}): "
                f"{result.stderr.strip()}"
            )
    except FileNotFoundError:
        return "Error: spelunk not found in PATH."
    except subprocess.TimeoutExpired:
        return "Error: spelunk command timed out."
    if len(output) > MAX_OUTPUT_CHARS:
        output = output[:MAX_OUTPUT_CHARS] + "\n... (output truncated)"
    return output or "(no results)"


def spelunk_search(repo_path: Path, query: str, limit: int = 10) -> str:
    return _run_spelunk(
        repo_path, ["search", query, "--limit", str(limit), "--format", "json"]
    )


def spelunk_graph(repo_path: Path, symbol: str) -> str:
    return _run_spelunk(repo_path, ["graph", symbol, "--format", "json"])


def spelunk_memory_search(repo_path: Path, query: str, limit: int = 10) -> str:
    return _run_spelunk(
        repo_path,
        ["memory", "search", query, "--limit", str(limit), "--format", "json"],
    )


def build_dispatch_table(repo_path: Path) -> dict:
    """Return {tool_name: callable(repo_path, arguments_json) -> str}."""
    return {
        "read_file": lambda args: read_file(repo_path, args["path"]),
        "run_bash": lambda args: run_bash(repo_path, args["command"]),
        "write_file": lambda args: write_file(repo_path, args["path"], args["content"]),
        "spelunk_search": lambda args: spelunk_search(
            repo_path, args["query"], args.get("limit", 10)
        ),
        "spelunk_graph": lambda args: spelunk_graph(repo_path, args["symbol"]),
        "spelunk_memory_search": lambda args: spelunk_memory_search(
            repo_path, args["query"], args.get("limit", 10)
        ),
    }


# ---------------------------------------------------------------------------
# Agent loop
# ---------------------------------------------------------------------------


def build_tools(condition: str) -> list[dict]:
    """Build the tool list for the given condition."""
    base = list(BASE_TOOLS)
    if condition == "baseline":
        return base
    elif condition == "spelunk_search":
        return base + [SPELUNK_SEARCH_TOOL]
    elif condition == "spelunk_full":
        return base + [
            SPELUNK_SEARCH_TOOL,
            SPELUNK_GRAPH_TOOL,
            SPELUNK_MEMORY_SEARCH_TOOL,
        ]
    else:
        raise ValueError(f"Unknown condition: {condition}")


def get_system_prompt(condition: str) -> str:
    """Return the appropriate system prompt for the condition."""
    if condition == "baseline":
        return SYSTEM_PROMPT_BASE
    return SYSTEM_PROMPT_SPELUNK


def get_spelunk_version() -> str:
    """Return spelunk version string, or 'unknown' if not available."""
    try:
        result = subprocess.run(
            ["spelunk", "--version"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except Exception:
        pass
    return "unknown"


def run_agent(
    task_id: str,
    repo_path: Path,
    issue_text: str,
    client: OpenAI,
    model: str,
    condition: str,
    max_turns: int,
    seed: int,
) -> dict:
    tools = build_tools(condition)
    system_prompt = get_system_prompt(condition)
    dispatch = build_dispatch_table(repo_path)

    messages = [
        {"role": "system", "content": system_prompt},
        {
            "role": "user",
            "content": (
                f"Repository path: {repo_path}\n\nIssue:\n{issue_text}\n\n"
                "Please investigate the issue and apply a fix."
            ),
        },
    ]

    turns = 0
    input_tokens = 0
    output_tokens = 0
    start = time.monotonic()

    while turns < max_turns:
        response = client.chat.completions.create(
            model=model,
            max_tokens=4096,
            tools=tools,
            tool_choice="auto",
            messages=messages,
            seed=seed,
        )
        msg = response.choices[0].message
        input_tokens += response.usage.prompt_tokens if response.usage else 0
        output_tokens += response.usage.completion_tokens if response.usage else 0
        turns += 1

        assistant_entry: dict = {"role": "assistant", "content": msg.content or ""}
        # DeepSeek thinking mode: preserve reasoning_content if present
        if hasattr(msg, "reasoning_content") and msg.reasoning_content:
            assistant_entry["reasoning_content"] = msg.reasoning_content
        if msg.tool_calls:
            assistant_entry["tool_calls"] = [
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
        messages.append(assistant_entry)

        if response.choices[0].finish_reason != "tool_calls" or not msg.tool_calls:
            break

        for tc in msg.tool_calls:
            name = tc.function.name
            handler = dispatch.get(name)
            if handler is None:
                result = f"Unknown tool: {name}"
            else:
                try:
                    args = json.loads(tc.function.arguments)
                    result = handler(args)
                except Exception as e:
                    result = f"Error dispatching {name}: {e}"
            messages.append({"role": "tool", "tool_call_id": tc.id, "content": result})

    return {
        "task_id": task_id,
        "resolved": False,  # determined externally by SWE-bench harness
        "turns": turns,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "wall_seconds": round(time.monotonic() - start, 2),
    }


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Unified SWE-bench agent (OpenAI-compatible API)."
    )
    parser.add_argument(
        "--condition",
        required=True,
        choices=["baseline", "spelunk_search", "spelunk_full"],
    )
    parser.add_argument("--task-id", required=True)
    parser.add_argument("--repo-path", required=True)
    parser.add_argument("--issue", required=True)
    parser.add_argument("--model", default="deepseek-v4-flash")
    parser.add_argument("--api-base-url", default="https://api.deepseek.com/v1")
    parser.add_argument(
        "--api-key",
        default=None,
        help="API key (falls back to DEEPSEEK_API_KEY env var)",
    )
    parser.add_argument("--max-turns", type=int, default=20)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument(
        "--save-patch",
        default=None,
        help="Save git diff to this file after agent finishes (for SWE-bench eval).",
    )
    args = parser.parse_args()

    repo_path = Path(args.repo_path).resolve()
    if not repo_path.is_dir():
        parser.error(f"repo-path does not exist: {repo_path}")

    # Resolve API key
    api_key = args.api_key or os.environ.get("DEEPSEEK_API_KEY")
    api_key_source = "flag:--api-key" if args.api_key else "env:DEEPSEEK_API_KEY"
    if not api_key:
        parser.error(
            "No API key provided. Use --api-key or set DEEPSEEK_API_KEY env var."
        )

    # Issue text can come from a file path or inline
    issue_text = args.issue
    issue_path = Path(issue_text)
    if issue_path.is_file():
        issue_text = issue_path.read_text()

    client = OpenAI(base_url=args.api_base_url, api_key=api_key)

    # Run the agent
    agent_result = run_agent(
        task_id=args.task_id,
        repo_path=repo_path,
        issue_text=issue_text,
        client=client,
        model=args.model,
        condition=args.condition,
        max_turns=args.max_turns,
        seed=args.seed,
    )

    # Save git diff for SWE-bench evaluation
    patch_path = None
    if args.save_patch:
        try:
            # Stage all changes excluding spelunk artifacts and ISSUE.txt.
            subprocess.run(
                ["git", "add", "-A", "--", ":!.spelunk", ":!ISSUE.txt"],
                cwd=repo_path,
                capture_output=True,
                text=True,
                timeout=30,
                check=True,
            )
            diff = subprocess.run(
                ["git", "diff", "--cached", "HEAD"],
                cwd=repo_path,
                capture_output=True,
                text=True,
                timeout=30,
                check=True,
            ).stdout
            patch_path = Path(args.save_patch)
            patch_path.parent.mkdir(parents=True, exist_ok=True)
            patch_path.write_text(diff)
        except Exception as e:
            print(f"Warning: failed to save patch: {e}", file=sys.stderr)

    # Reproducibility contract
    output = {
        "benchmark": "swebench-verified",
        "condition": args.condition,
        "model": args.model,
        "model_source": "api",
        "api_base_url": args.api_base_url,
        "api_key_source": api_key_source,
        "spelunk_version": get_spelunk_version(),
        "seed": args.seed,
        "max_turns": args.max_turns,
        "patch_file": str(patch_path) if patch_path else None,
        **agent_result,
    }
    print(json.dumps(output))


if __name__ == "__main__":
    main()
