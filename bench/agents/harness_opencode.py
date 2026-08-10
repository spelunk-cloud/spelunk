#!/usr/bin/env python3
"""SWE-bench single-task runner — opencode harness, headless.

Sibling of agent.py, but instead of our own OpenAI-compatible tool-calling
loop, this shells out to the `opencode` CLI in headless mode (`opencode run`)
so the same (task, model, condition) cell can be compared across harnesses.
Only the harness varies — everything else (task repo, issue text, model,
patch-extraction convention) is held constant (bench/AGENTS.md principle #1).

DeepSeek is wired in via opencode's native custom-provider mechanism: a
generated `opencode.json` in the task repo registers a
"deepseek" provider using the @ai-sdk/openai-compatible npm adapter, pointed
at DeepSeek's own /v1 endpoint (not the Anthropic-compat one — that's the
claude-code harness's path). See bench/agents/README.md for the exact
provider block.

Usage:
    python bench/agents/harness_opencode.py \\
        --task-id django__django-11099 \\
        --repo-path /path/to/repo \\
        --issue bench/repos/django__django-11099/ISSUE.txt \\
        --model deepseek-v4-flash \\
        --api-base-url https://api.deepseek.com/v1 \\
        --api-key "$DEEPSEEK_API_KEY" \\
        [--max-turns 20] [--seed 42] [--save-patch bench/patches/.../task.patch]

Output: single JSON object on stdout (same reproducibility contract as
agent.py, plus the harness-dimension fields — see bench/agents/README.md).
"""

import argparse
import json
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

from agent import get_spelunk_version
from harness_common import CONDITIONS, build_system_prompt, extract_patch, read_issue_text
from spelunk_mcp_server import (
    SERVER_NAME,
    SPELUNK_CONDITIONS,
    mcp_server_command,
    mcp_tool_names_for_condition,
    read_telemetry,
)

PROVIDER_ID = "spelunk-bench-deepseek"

OPENCODE_SYSTEM_PROMPT = (
    "You are an expert software engineer. You are given a GitHub issue and a "
    "repository checkout. Your goal is to produce a minimal patch that fixes the "
    "issue. Explore the codebase, understand the problem, and apply the fix "
    "directly by editing files in the repository. When you are done, briefly "
    "summarise what you changed."
)


def get_opencode_command() -> list[str]:
    """Resolve how to invoke opencode: prefer a binary already on PATH (the
    steady-state expectation once installed), otherwise fall back to `npx`
    so the adapter still works in an environment where it's merely
    available via npm but not globally installed."""
    if shutil.which("opencode"):
        return ["opencode"]
    return ["npx", "--yes", "opencode-ai@latest"]


def get_opencode_version(opencode_cmd: list[str]) -> str:
    try:
        result = subprocess.run(
            [*opencode_cmd, "--version"],
            capture_output=True,
            text=True,
            timeout=60,
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except Exception:
        pass
    return "unknown"


def write_provider_config(
    repo_path: Path,
    model: str,
    api_base_url: str,
    api_key: str,
    condition: str = "baseline",
    telemetry_log: Path | None = None,
) -> Path:
    """Write an opencode.json in the task repo registering DeepSeek as a
    custom OpenAI-compatible provider (native DeepSeek endpoint — not the
    Anthropic-compat shim used by the claude-code harness), plus the spelunk
    MCP server on a spelunk condition.

    Scoped to the repo directory (not global ~/.config/opencode/) so
    concurrent/parallel task runs never race on a shared config file, and so
    nothing about the host environment is mutated by a bench run.
    """
    config = {
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            PROVIDER_ID: {
                "npm": "@ai-sdk/openai-compatible",
                "name": "DeepSeek (spelunk-bench)",
                "options": {
                    "baseURL": api_base_url,
                    "apiKey": api_key,
                },
                "models": {
                    model: {"name": model},
                },
            }
        },
    }
    if condition in SPELUNK_CONDITIONS:
        config["mcp"] = {
            SERVER_NAME: {
                "type": "local",
                "command": mcp_server_command(condition, repo_path, telemetry_log),
                "environment": {"SPELUNK_SECRET_STORE": "file"},
                "enabled": True,
            }
        }
    config_path = repo_path / "opencode.json"
    config_path.write_text(json.dumps(config, indent=2))
    return config_path


def run_opencode(
    repo_path: Path,
    issue_text: str,
    model: str,
    api_base_url: str,
    api_key: str,
    max_turns: int,  # accepted for CLI-contract parity; opencode has no
    # per-task turn cap of its own, see README "Adapter notes"
    opencode_cmd: list[str],
    condition: str = "baseline",
) -> dict:
    system_prompt = build_system_prompt(
        OPENCODE_SYSTEM_PROMPT,
        condition,
        mcp_tool_names_for_condition(condition) if condition in SPELUNK_CONDITIONS else [],
    )
    prompt = (
        f"{system_prompt}\n\n"
        f"Repository path: {repo_path}\n\nIssue:\n{issue_text}\n\n"
        "Please investigate the issue and apply a fix."
    )

    cmd = [
        *opencode_cmd,
        "run",
        "--dir",
        str(repo_path),
        "--model",
        f"{PROVIDER_ID}/{model}",
        "--format",
        "json",
        "--auto",  # auto-approve permissions — required for headless runs
        prompt,
    ]

    start = time.monotonic()
    result = subprocess.run(
        cmd,
        cwd=repo_path,
        capture_output=True,
        text=True,
        timeout=900,
    )
    wall_seconds = round(time.monotonic() - start, 2)

    turns = 0
    input_tokens = 0
    output_tokens = 0
    parse_error = None

    # --format json emits one JSON event per line. opencode 1.17.13 uses:
    #   {"type":"step_finish", ..., "part":{"tokens":{"input":N,"output":N,...}}}
    # We don't depend on a specific event schema beyond counting
    # turn-completion events and summing token usage from their payload, so
    # a future opencode version that adds fields doesn't break us
    # (only a field *rename* would silently zero these out — see README).
    for line in result.stdout.splitlines():
        line = line.strip()
        if not line or not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        role = event.get("type") or event.get("role")
        if role in ("step_finish", "step", "assistant", "message"):
            turns += 1
        tokens = (
            (event.get("part") or {}).get("tokens")
            or event.get("usage")
            or event.get("tokens")
        )
        if isinstance(tokens, dict):
            input_tokens += int(tokens.get("input", tokens.get("prompt_tokens", 0)) or 0)
            output_tokens += int(tokens.get("output", tokens.get("completion_tokens", 0)) or 0)

    if result.returncode != 0 and turns == 0:
        parse_error = (result.stderr or result.stdout)[:2000]

    return {
        "resolved": False,  # determined externally by SWE-bench harness
        "turns": turns,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "wall_seconds": wall_seconds,
        "harness_exit_code": result.returncode,
        "harness_error": parse_error,
    }


def main() -> None:
    parser = argparse.ArgumentParser(
        description="SWE-bench single-task runner — opencode harness (headless)."
    )
    parser.add_argument(
        "--condition",
        default="baseline",
        choices=list(CONDITIONS),
        help=(
            "Recorded verbatim in provenance as condition. On a spelunk "
            "condition the spelunk tools are wired in over a bench-local MCP "
            "server — see bench/agents/README.md."
        ),
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
        help="Save git diff to this file after the run finishes.",
    )
    args = parser.parse_args()

    repo_path = Path(args.repo_path).resolve()
    if not repo_path.is_dir():
        parser.error(f"repo-path does not exist: {repo_path}")

    api_key = args.api_key or os.environ.get("DEEPSEEK_API_KEY")
    provenance_label = "flag:--api-key" if args.api_key else "env:DEEPSEEK_API_KEY"
    if not api_key:
        parser.error(
            "No API key provided. Use --api-key or set DEEPSEEK_API_KEY env var."
        )

    issue_text = read_issue_text(args.issue)

    opencode_cmd = get_opencode_command()
    opencode_version = get_opencode_version(opencode_cmd)

    scratch_dir = Path(tempfile.mkdtemp(prefix="spelunk-bench-mcp-"))
    telemetry_log = scratch_dir / "tool_calls.jsonl"

    config_path = write_provider_config(
        repo_path,
        args.model,
        args.api_base_url,
        api_key,
        condition=args.condition,
        telemetry_log=telemetry_log,
    )
    try:
        agent_result = run_opencode(
            repo_path=repo_path,
            issue_text=issue_text,
            model=args.model,
            api_base_url=args.api_base_url,
            api_key=api_key,
            max_turns=args.max_turns,
            opencode_cmd=opencode_cmd,
            condition=args.condition,
        )
        # Read before the scratch dir goes away.
        telemetry = read_telemetry(telemetry_log)
    finally:
        # Don't leave bench-generated provider config in the task repo's
        # working tree — it would otherwise show up in the saved patch
        # despite being outside SOURCE_PATHSPECS (harmless there, but
        # cleaner to remove) and would leak the API key into repo state.
        config_path.unlink(missing_ok=True)
        shutil.rmtree(scratch_dir, ignore_errors=True)

    patch_path = extract_patch(repo_path, args.save_patch)

    output = {
        "benchmark": "swebench-verified",
        "condition": args.condition,
        "harness": "opencode",
        "harness_version": opencode_version,
        "endpoint_kind": "native",
        # opencode has no effort/thinking concept of its own (that's a
        # claude-code-harness-only knob) -- always null here so every
        # harness's result JSON is a strict key-superset of the documented
        # provenance contract (bench/agents/README.md "Reproducibility /
        # provenance contract"), never a per-harness subset.
        "effort": None,
        "thinking": None,
        "model": args.model,
        "model_source": "api",
        "api_base_url": args.api_base_url,
        "api_key_source": provenance_label,
        # Null only on baseline, where no spelunk tools are wired in.
        "spelunk_version": (
            get_spelunk_version() if args.condition in SPELUNK_CONDITIONS else None
        ),
        "seed": args.seed,
        "run_seed": args.seed,
        "max_turns": args.max_turns,
        "task_id": args.task_id,
        "patch_file": str(patch_path) if patch_path else None,
        # Populated later, once the corresponding infra lands (README §Provenance):
        "question_set_version": None,
        "instance_filter": None,
        "judge_model": None,
        "judge_version": None,
        "judge_error_rate": None,
        **telemetry,
        **agent_result,
    }
    print(json.dumps(output))


if __name__ == "__main__":
    main()
