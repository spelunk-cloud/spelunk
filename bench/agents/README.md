# Agents — SWE-bench Agent Scripts

Unified agent for running SWE-bench tasks via any OpenAI-compatible API
(DeepSeek, self-hosted, or local LM Studio), with a harness dimension so the
same (task, model, condition) cell can be run under three different coding
agents.

## Quick Start

```bash
# 1. Set up repos (one-time)
bash bench/setup_repos.sh --tasks 5

# 2. Run the agent on a single task (harness=none, the component-clean cell)
python bench/agents/agent.py \
    --condition baseline \
    --task-id django__django-11099 \
    --repo-path bench/repos/django__django-11099 \
    --issue bench/repos/django__django-11099/ISSUE.txt \
    --api-key "$DEEPSEEK_API_KEY"

# 3. Run the full 50-task benchmark + Docker evaluation in one step
bash bench/agents/swebench_run.sh \
    --condition spelunk_full \
    --harness none \
    --api-key "$DEEPSEEK_API_KEY" \
    --eval

# 3b. Agent run only (evaluate later)
bash bench/agents/swebench_run.sh \
    --condition spelunk_full \
    --harness none \
    --api-key "$DEEPSEEK_API_KEY"
# The script prints the swebench_eval.sh command to run next.
```

`swebench_run.sh` is the **canonical batch orchestrator** for this
directory. `batch_run.py` is a retired duplicate (it hardcoded
`~/opensource/spelunk-bench/repos` with no override and had drifted out of
sync with `setup_repos.sh`'s repo-dir convention) — do not use it for new
runs; it is kept only so old invocations in scrollback don't 404.

## Conditions

| Condition | Tools |
|-----------|-------|
| `baseline` | `read_file`, `run_bash`, `write_file` |
| `spelunk_search` | baseline + `spelunk_search` (semantic code retrieval) |
| `spelunk_full` | baseline + `spelunk_search` + `spelunk_graph` + `spelunk_memory_search` |

`condition` and `harness` are independent dimensions — vary exactly one
at a time between comparisons (bench/AGENTS.md principle #1). All three
conditions run under all three harnesses. `--harness none` (`agent.py`) calls
the spelunk tools in-process; opencode and claude-code reach the same tools
over `spelunk_mcp_server.py`, a bench-local stdio MCP server that *imports*
`agent.py`'s tool functions and schemas rather than reimplementing them, so
the capability behind a `spelunk` condition cannot drift between harnesses.
Each harness's result JSON records `condition` from `--condition`, so the
value in the JSON always matches what was requested. (The
DeepSeek-vs-native-Claude distinction for the claude-code harness lives in
`endpoint_kind`, not `condition` — see the provenance table below.)

**Tool names differ by harness; capability does not.** Under `--harness none`
the model sees `spelunk_search`; under the MCP harnesses it sees
`mcp__spelunk__spelunk_search`. Names, descriptions, and schemas are
otherwise identical because they are the same objects. Transport differs,
capability does not.

**The system prompt is not byte-identical across all three harnesses.**
There are two distinct base prompts, not three: `opencode` and `claude-code`
share a byte-identical base, and `agent.py` (`--harness none`) differs from
them by one clause describing how that harness applies edits. `agent.py` edits
only through its own tool dispatch ("use the available tools ... and apply the
fix"); the MCP harnesses edit with their native editing loop ("apply the fix
directly by editing files in the repository"). That clause is load-bearing, so
the prompts are deliberately *not* unified: giving either harness the other's
sentence would describe an edit mechanism it does not have.

On a spelunk condition each harness appends a guidance sentence naming its own
tool names rather than reusing `agent.py`'s whole `SYSTEM_PROMPT_SPELUNK`. The
sentence is held as an exact substring of `SYSTEM_PROMPT_SPELUNK` and asserted
by the offline suite, so an upstream reword fails loudly.

What that means when reading a result:

- *Within* a harness the baseline/spelunk contrast is clean. The baseline arm
  gets that harness's base prompt verbatim, the spelunk arm gets the same base
  plus the guidance sentence, so the base cancels out of the uplift.
- `opencode` vs `claude-code` spelunk uplift is also prompt-clean: identical
  base, identical guidance core, differing only in tool names, which MCP
  namespacing forces.
- Only comparisons *against* `--harness none` carry the one-clause difference,
  and there it is one of several irreducible differences between an in-process
  agent and a subprocess one. Weigh it there, not across the MCP harnesses.

**MCP hygiene (`--strict-mcp-config`).** The claude-code adapter passes
`--strict-mcp-config` in *both* arms, so only the bench's own `--mcp-config`
is loaded. A dev host with its own MCP servers configured would otherwise
leak them into baseline *and* spelunk, contaminating both. opencode has no
equivalent flag; its generated `opencode.json` is repo-scoped, and on
v1.17.20 `mcp` servers declared in a global `~/.config/opencode/` config were
observed not to spawn for a repo-scoped run. That is an observed behaviour,
not a documented guarantee. Unlike `--strict-mcp-config`, nothing enforces it,
so re-check it when bumping opencode.

**Tool-invocation telemetry.** Runs on a spelunk condition report
`spelunk_mcp_server_spawned` plus `spelunk_tool_calls` /
`spelunk_tool_calls_by_tool`. These separate three outcomes that otherwise
look alike: the server never spawned (broken wiring, i.e. a baseline run with
extra latency, not a spelunk cell), it spawned but the model never reached
for a tool (a real result), or it was used.

## Harnesses

`--harness none|opencode|claude-code` (`swebench_run.sh` and each
single-task runner script). Same task repo, issue text, model identity, and
patch-extraction convention across all three — only the harness varies.

| Harness | Runner script | What it shells out to |
|---|---|---|
| `none` | `agent.py` | Nothing external — this repo's own OpenAI-compatible tool-calling loop. The component-clean cell: no external agent framework in the loop at all. |
| `opencode` | `harness_opencode.py` | Headless `opencode run` (or `npx opencode-ai@latest run` if `opencode` isn't on PATH). |
| `claude-code` | `harness_claude_code.py` | Headless `claude -p`. |

### `agent.py` — Single-task runner (harness=none)

```bash
python bench/agents/agent.py \
    --condition baseline|spelunk_search|spelunk_full \
    --task-id <task_id> \
    --repo-path /path/to/repo \
    --issue "Issue text or path to ISSUE.txt" \
    --model deepseek-v4-flash \
    --api-base-url https://api.deepseek.com/v1 \
    --api-key "$DEEPSEEK_API_KEY" \
    [--max-turns 20] [--seed 42]
```

The `--issue` argument accepts either inline text or a file path. If the
argument points to an existing file, its contents are read as the issue text.

### `harness_opencode.py` — Single-task runner (harness=opencode)

```bash
python bench/agents/harness_opencode.py \
    --task-id <task_id> \
    --repo-path /path/to/repo \
    --issue "Issue text or path to ISSUE.txt" \
    --model deepseek-v4-flash \
    --api-base-url https://api.deepseek.com/v1 \
    --api-key "$DEEPSEEK_API_KEY" \
    [--max-turns 20] [--seed 42]
```

DeepSeek is wired in via opencode's own native custom-provider mechanism, not
a compat shim: the script writes a scratch `opencode.json` into the task repo
(removed again once the run finishes) registering a `spelunk-bench-deepseek`
provider —

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "spelunk-bench-deepseek": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "DeepSeek (spelunk-bench)",
      "options": { "baseURL": "https://api.deepseek.com/v1", "apiKey": "..." },
      "models": { "deepseek-v4-flash": { "name": "deepseek-v4-flash" } }
    }
  }
}
```

— then runs `opencode run --dir <repo> --model spelunk-bench-deepseek/<model>
--format json --auto <prompt>`. `--auto` auto-approves permissions (required
for a headless run — there is no TTY to answer a permission prompt).
Patch extraction is the same git-diff-of-the-working-tree approach as
`agent.py` (spec point 3) — see `harness_common.py`.

**Adapter notes:**
- opencode has no first-class per-task turn cap; `--max-turns` is accepted
  for CLI-contract parity across harnesses but is not enforced by opencode
  itself. `--max-turns` is still recorded in provenance for the record, but
  don't read it as an enforced ceiling for this harness the way it is for
  `agent.py`.
- Turn/token counts are parsed from `--format json`'s per-line event stream
  on a best-effort basis (matching on `role`/`type` and `usage`/`tokens`
  keys). A future opencode version that **renames** those fields (rather
  than adding new ones) would silently zero out `turns`/`input_tokens`/
  `output_tokens` — if those come back as 0 on an otherwise-successful run,
  check `--format json`'s actual event shape against what this script parses
  before trusting the numbers.
- The generated `opencode.json` never gets committed or included in the
  saved patch (it isn't a recognised source extension, and the script
  deletes it in a `finally` block regardless) — do not point `--dir` at a
  repo that already carries its own `opencode.json`; the scratch config
  will clobber it for the duration of the run.

### `harness_claude_code.py` — Single-task runner (harness=claude-code)

```bash
python bench/agents/harness_claude_code.py \
    --task-id <task_id> \
    --repo-path /path/to/repo \
    --issue "Issue text or path to ISSUE.txt" \
    --model deepseek-v4-flash \
    --api-key "$DEEPSEEK_API_KEY" \
    [--effort high] [--thinking] [--max-turns 20] [--seed 42]
```

DeepSeek is reached via its documented Anthropic-compatible endpoint, using
env overrides (verified against DeepSeek's live docs — see "DeepSeek
endpoint verification" below):

```bash
export ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic
export ANTHROPIC_AUTH_TOKEN=$DEEPSEEK_API_KEY   # note: AUTH_TOKEN, not API_KEY
export ANTHROPIC_MODEL=deepseek-v4-flash
```

The script sets these itself (plus `ANTHROPIC_API_KEY` as a defensive
belt-and-braces alias — see the docstring in `harness_claude_code.py` for
why) in the subprocess env only; nothing is exported into the calling
shell or written to disk.

**Shim fallback policy:** if the Anthropic-compat endpoint misbehaves (e.g.
malformed tool-call turns, streaming issues), pass
`--endpoint-kind shim --shim-base-url http://<host>:<port>` to point Claude
Code's `ANTHROPIC_BASE_URL` at an already-running Anthropic->OpenAI proxy
(e.g. LiteLLM) in front of DeepSeek's OpenAI-compatible endpoint instead.
This script does **not** start the proxy process itself — bring your own.
Every result JSON records which path was used via `endpoint_kind`:
`"native"` (no DeepSeek — Claude Code's own ambient Anthropic credentials,
via `--no-deepseek`), `"anthropic-compat"` (DeepSeek's own compat endpoint,
the default), or `"shim"` (proxy in front). Never conflate `anthropic-compat`
and `shim` results in a comparison — they are, deliberately, not the same
condition.

**Effort/thinking (future Claude-model cells):** `--effort` is **always**
pinned (default `high`) and recorded in provenance as `effort`, and
`--thinking` is recorded as a boolean `thinking` field, so that a future cell
running an actual Claude model through this same harness stays reproducible
without a separate schema change.

**Adapter notes:**
- Headless runs use `--permission-mode acceptEdits` (accepts file edits
  without a TTY prompt) and `--output-format json` (single JSON result
  object, not a stream) — turns/tokens are read from `num_turns` and
  `usage.input_tokens` + `usage.cache_creation_input_tokens` /
  `usage.output_tokens` in that result object.
- `--no-deepseek` runs Claude Code against its own ambient Anthropic
  credentials (whatever `claude` is already authenticated with in the
  environment) and skips all DeepSeek env overrides — this is the escape
  hatch for exercising the harness plumbing without a DeepSeek key, and the
  intended path for future native-Claude-model cells.
- Like the opencode harness, `--max-turns` is accepted and recorded in
  provenance for CLI-contract parity across harnesses, but is **not
  enforced** — it is never passed to the `claude -p` subprocess. Don't read
  it as an enforced ceiling for this harness either.
- On the DeepSeek path (anthropic-compat and shim, not `--no-deepseek`), the
  `claude -p` subprocess runs with `CLAUDE_CONFIG_DIR` pointed at a fresh,
  empty scratch directory instead of the host's default config dir. Without
  this, a `claude` binary that is already logged in on the host sends its
  stored OAuth credential instead of the injected
  `ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_API_KEY`, and DeepSeek returns 401 with
  no indication the token was ignored. The scratch dir is deleted once the
  run finishes, same lifecycle as the MCP scratch dir above.

## swebench_run.sh — Batch orchestrator

```bash
bash bench/agents/swebench_run.sh \
    --condition spelunk_full \
    --harness none \
    --model deepseek-v4-flash \
    --api-key "$DEEPSEEK_API_KEY" \
    [--tasks 50] [--max-turns 20] [--seed 42] [--skip-index] [--eval]

# opencode harness
bash bench/agents/swebench_run.sh \
    --condition baseline --harness opencode \
    --api-key "$DEEPSEEK_API_KEY" --tasks 5

# claude-code harness, DeepSeek via the Anthropic-compat endpoint
bash bench/agents/swebench_run.sh \
    --condition baseline --harness claude-code \
    --api-key "$DEEPSEEK_API_KEY" --effort high --tasks 5

# claude-code harness, native Claude model (no DeepSeek)
bash bench/agents/swebench_run.sh \
    --condition baseline --harness claude-code --no-deepseek \
    --model claude-sonnet-5 --effort high --tasks 5
```

Reads `bench/agents/tasks_50.json`, expects repos checked out at
`bench/repos/<task_id>/` by default — or, if
`~/opensource/spelunk-bench/repos` exists, that shared checkout instead
(same convention as `bench/setup_repos.sh`, so both scripts always agree on
where repos live without needing `--repos-dir` on every invocation).
Override either with `--repos-dir`.

For `spelunk_search`/`spelunk_full` conditions, runs `spelunk index` (and,
for `spelunk_full`, `spelunk memory harvest`) on each repo before the agent,
unless `--skip-index` is set. This is gated on the condition, not the
harness: every harness reaches the same tools on a spelunk condition, and
without the index the spelunk arm is a silent no-op that scores like
baseline.

Each task's patch is saved to
`bench/patches/<condition>-<timestamp>/<task_id>.patch` for `--harness none`,
or `bench/patches/<condition>-<harness>-<timestamp>/<task_id>.patch` for the
other two (override either with `--patches-dir`). These patches are required
for the Docker harness.

Results are written to `bench/results/swebench-<condition>-<timestamp>.json`
for `--harness none` (unchanged filename convention — additive only, so
existing tooling/scripts that glob for this pattern keep working), or
`bench/results/swebench-<condition>-<harness>-<timestamp>.json` for
`opencode`/`claude-code`.

Pass `--eval` to automatically invoke `swebench_eval.sh` after the agent run
completes, computing real `resolve_rate` via the SWE-bench Docker harness.
Without `--eval`, the script prints the exact command to run next.

## Reproducibility / provenance contract

Every result JSON includes the original reproducibility fields, plus the
harness-matrix provenance extension (all additive — `--harness none` output
is a strict superset of the pre-harness-matrix schema, so existing consumers
that read specific keys via `.get()` — `export_patches.py`, `report.py` — are
unaffected):

```json
{
    "benchmark": "swebench-verified",
    "condition": "spelunk_full",
    "harness": "none",
    "harness_version": null,
    "endpoint_kind": "native",
    "effort": null,
    "thinking": null,
    "model": "deepseek-v4-flash",
    "model_source": "api",
    "api_base_url": "https://api.deepseek.com/v1",
    "api_key_source": "env:DEEPSEEK_API_KEY",
    "spelunk_version": "0.9.2",
    "seed": 42,
    "run_seed": 42,
    "max_turns": 20,
    "task_id": "django__django-11099",
    "patch_file": "bench/patches/spelunk_full-20260704T120000Z/django__django-11099.patch",
    "question_set_version": null,
    "instance_filter": null,
    "judge_model": null,
    "judge_version": null,
    "judge_error_rate": null,
    "turns": 5,
    "input_tokens": 12000,
    "output_tokens": 1500,
    "wall_seconds": 45.2,
    "resolved": false
}
```

New fields:

| Field | Meaning | Populated by |
|---|---|---|
| `harness` | `none`\|`opencode`\|`claude-code` | all three runners |
| `harness_version` | `opencode --version` / `claude --version` output; `null` for harness=none | `harness_opencode.py`, `harness_claude_code.py` |
| `effort` | Claude Code `--effort` level (`low`\|`medium`\|`high`\|`xhigh`\|`max`); `null` for non-claude-code harnesses | `harness_claude_code.py` |
| `thinking` | Claude Code thinking requested (bool); `null` for non-claude-code harnesses | `harness_claude_code.py` |
| `endpoint_kind` | `native`\|`anthropic-compat`\|`shim` — which wire format/endpoint reached the model | all three runners |
| `run_seed` | Alias of `seed`, always present alongside it — the field name every harness's provenance dict should read to line up seeds across a run regardless of which runner produced the row | all three runners |
| `question_set_version` | Reserved, `null` today — will identify the question-set revision once judge-based benchmarks (Phase 6+) land | reserved |
| `instance_filter` | Reserved, `null` today — will record any task-subset filter applied | reserved |
| `judge_model` / `judge_version` / `judge_error_rate` | Reserved, `null` today — will record the LLM-judge identity/version and its measured error rate once judge-based scoring lands | reserved |
| `harness_exit_code` | Exit code of the underlying `opencode`/`claude` process | `harness_opencode.py`, `harness_claude_code.py` |
| `harness_error` | Best-effort error text if the harness process failed or its output couldn't be parsed; `null` on a clean run | `harness_opencode.py`, `harness_claude_code.py` |
| `resolved_model_usage` | Claude Code's own `modelUsage` breakdown (per-model token/cost, e.g. if it internally used a smaller model for a subagent step); `null` for the other harnesses | `harness_claude_code.py` |

`harness=none` rows keep `harness_version: null`, `effort: null`,
`thinking: null` — there is no separate "harness" version to report beyond
`spelunk_version` (already present), and no effort/thinking concept in
agent.py's own loop.

Anyone with a DeepSeek API key can reproduce a harness=none run:
```bash
export DEEPSEEK_API_KEY=sk-...
bash bench/agents/swebench_run.sh --condition spelunk_full --harness none --seed 42
```

## Contamination control: leakage-filtered instances

Track-B (SWE-bench) numbers are reported on two instance sets, **always
separately**, and every published figure names its `instance_filter`:

| `instance_filter`          | Instance set                                              |
|----------------------------|----------------------------------------------------------|
| `full`                     | SWE-bench Verified, unfiltered (500 instances)           |
| `swebench_plus_filtered`   | Verified minus SWE-Bench+ leakage/suspicious instances   |

**Why.** SWE-Bench+ (arXiv:2410.06992) found ~32.67% of passing SWE-bench
patches benefited from *solution leakage*: the fix appears in the issue report
or comments. Also, a large share of *suspicious* passes on weak tests (55.36%
of the Verified sample they inspected). A resolve_rate on the full set is
inflated by these. Reporting a `swebench_plus_filtered` figure alongside `full`
shows how much of a result survives contamination control.

**Reporting rule.** Never publish a single blended Track-B number. Every figure
carries its `instance_filter`; `full` and `swebench_plus_filtered` are reported
side by side.

### Generating the filtered list

`build_filtered_tasks.py` intersects Verified with a SWE-Bench+ exclude set and
writes `tasks_filtered.json` (with a provenance header):

```bash
python bench/agents/build_filtered_tasks.py \
    --labels swebench_plus_verified_exclude.json \
    --labels-source "arXiv:2410.06992 replication pkg, rev <sha/date>" \
    --out bench/agents/tasks_filtered.json
```

`--labels` is the SWE-Bench+ per-instance leakage/suspicious label set for
Verified (list of instance_ids to exclude, or an `id -> reason` map). SWE-Bench+
publishes a new post-cutoff dataset rather than a single filtered-Verified file,
so a maintainer must obtain these labels from the authors' released artifact and
pin the revision via `--labels-source`. Target survivor count is 150–300; the
script warns if the intersection falls outside that band. `--dry-run` reports
counts and the `tasks_50.json` overlap without writing.

> **Status:** `tasks_filtered.json` is **not yet committed**. It requires the
> SWE-Bench+ label set, which is not distributed as a fetchable file. The script
> above generates it once that input is supplied. Do not hand-author the list.

### Overlap with `tasks_50.json`

Of the 50-slice, **24** instances are in SWE-bench Verified (the other 26 come
from the SWE-bench *full* split; see `setup_repos.sh`, issue #252). Only those
24 can ever survive the filter; the survivor subset is reported by
`build_filtered_tasks.py --overlap-with bench/agents/tasks_50.json` once the
label set is available.

## DeepSeek endpoint verification

DeepSeek's Anthropic-compatible endpoint details were verified live against
`api-docs.deepseek.com` (guides/anthropic_api and
quick_start/agent_integrations/claude_code) on 2026-07-04:

- Endpoint: `https://api.deepseek.com/anthropic`
- Env vars: `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN` (not
  `ANTHROPIC_API_KEY` — the docs are explicit about `AUTH_TOKEN`),
  `ANTHROPIC_MODEL`
- Model mapping: passing `claude-sonnet-*`/`claude-haiku-*` maps server-side
  to `deepseek-v4-flash`; `claude-opus-*` maps to `deepseek-v4-pro`. Passing
  `deepseek-v4-flash` directly (what `harness_claude_code.py` does) works
  without relying on that mapping.

## Endpoint-fidelity sanity check (DeepSeek native vs. Anthropic-compat)

Not run in this pass: exercising it requires a live `DEEPSEEK_API_KEY`,
which was not available in the environment this infra was built in (see
"What actually ran" below). Once a key is available, run the same task
through both endpoints and diff tool-call behaviour:

```bash
export DEEPSEEK_API_KEY=sk-...

# Native OpenAI-compatible endpoint, via harness=none
python bench/agents/agent.py --condition baseline \
    --task-id <task_id> --repo-path <repo> --issue <repo>/ISSUE.txt \
    --model deepseek-v4-flash --api-base-url https://api.deepseek.com/v1 \
    --api-key "$DEEPSEEK_API_KEY" --seed 42

# Anthropic-compat endpoint, via harness=claude-code
python bench/agents/harness_claude_code.py \
    --task-id <task_id> --repo-path <repo> --issue <repo>/ISSUE.txt \
    --model deepseek-v4-flash --api-key "$DEEPSEEK_API_KEY" --seed 42
```

Compare: number of tool-call turns for the same issue, whether the model
front-loads exploration the same way, and whether the final patch differs in
substance (not just formatting). Record findings here once run — this
section is a placeholder for that write-up, not a result.

## Testing

The harness matrix is covered by a pytest suite in `bench/agents/tests/`:

```bash
uv run --with pytest pytest bench/agents/tests/ -v
```

Tests are fully offline — no API keys, network, or external harness binaries
(opencode/claude) required. Coverage includes:
- `test_harness_common.py`: `extract_patch` (normal/regression/no-changes cases; `git add` silent-null bug fix validation)
- `test_harness_opencode.py`: `write_provider_config`, `get_opencode_command`
- `test_swebench_run_args.py`: argument validation (`--harness` enum, `--endpoint-kind`, `--no-deepseek`)
- `test_provenance_contract.py`: harness-matrix provenance fields (additive-only contract verification), exercised against all three harnesses — `agent.py` (harness=none, via a stubbed `openai`/`dotenv` import — no real dependency required), `harness_opencode.py`, and `harness_claude_code.py` (both via a fake binary shimmed onto `PATH`)

## aggregate_telemetry.py — per-cell token/cost table

Turns the raw result JSONs in `bench/results/` into a per-cell telemetry and
cost table. Pure Python, stdlib only — no API keys, no DB, no network.

```bash
python bench/agents/aggregate_telemetry.py            # prints the markdown table
python bench/agents/aggregate_telemetry.py \
    --results-dir bench/results \
    --prices bench/agents/pricing.json \
    --json-out telemetry.json --md-out telemetry.md
```

A **cell** is `(model, harness, condition, instance_filter)`. Result rows are
grouped by cell; legacy rows with no `harness` field (pre-harness-matrix) are
treated as `harness: none`, so `agent.py` output and the harness-adapter output
aggregate side by side without conflating harnesses. Per cell it reports task
count and mean/median input tokens, output tokens, turns, and wall seconds —
per-harness numbers stay separate.

Framed as tokens-to-outcome, never a headline "tokens saved" (binding P8).

### Cost extrapolation and pricing

Prices live in a committed config (`bench/agents/pricing.json`, override with
`--prices`). Every price carries a `verified_on` date; a `null` price is a
placeholder that yields **no** cost estimate (the cell is marked `Priced: no`)
rather than a silent zero — prices are never hardcoded in the script. The
shipped config carries Sonnet 5 and Opus 4.8 list prices (Sonnet 5 also notes
its intro pricing through 2026-08-31) and DeepSeek V4 Flash/Pro list prices
(verified 2026-07-10).

Per cell, `raw $` = `Σ_rows (input_tokens × P_in + output_tokens × P_out)`.
Rows already span every seed present, so there is no `n_seeds` multiplier here.

**Cache-aware effective rate:** where a row carries `cache_read_input_tokens`,
that portion of the input is re-billed at the cache-read rate (~0.1× the input
price, or a per-model `cache_read_per_mtok`); both raw and effective cost are
reported. Rows without the field bill effective == raw.

**Projections** for prospective (not-yet-run) cells live in the `projections`
list of the price config: `cost ≈ tasks × conditions × seeds ×
(input_tokens_per_task × P_in + output_tokens_per_task × P_out)`. The shipped
config includes `Sonnet-5 × 50-slice × 2 conditions × 3 seeds`. Per-task token
counts there are **estimates** — replace with measured means from a pilot cell
before quoting a figure.

Tests: `uv run --with pytest pytest bench/agents/tests/ -v` (offline; covers
grouping, legacy-none handling, cost/cache math, and projection). The committed
`bench/results/examples/swebench-harness-matrix-fixture.json` carries the
harness-matrix provenance fields so aggregation over a harness-carrying file is
exercised end to end.

## Notes

- `resolved` is always `false` in agent output from every runner — resolution
  comes from the SWE-bench Docker harness. Use `--eval` on `swebench_run.sh`
  or run `swebench_eval.sh` separately to populate real resolve rates.
- The spelunk CLI must be in PATH for `--harness none`. The agent handles
  exit code 1 (no results) gracefully.
- DeepSeek API may have rate limits — the orchestrator pauses 1 s between tasks.
- **Infrastructure vs. resolve_rate:** Infrastructure fixes (Phase 3) unblock
  benchmarks by ensuring tasks run without crashes. They do not improve
  `resolve_rate` — that requires a capable model (deepseek-v4-flash).
- **spelunk_full vs spelunk_search:** For SWE-bench repos checked out at single
  commits, `spelunk memory harvest` has no git history — memory tools return
  empty results. `spelunk_full` is equivalent to `spelunk_search` for these
  repos. The condition differentiates only on repos with prior spelunk memory
  (Phase 6 benchmarks).
- **Phase 6a prerequisite:** `spelunk context` (#201) must be merged before the
  cross-session handoff benchmark can be scripted as described in the plan.
