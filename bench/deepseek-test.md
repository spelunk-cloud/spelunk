Everything checks out, with one gotcha I can save you: the #518 branch is 30 commits behind main and still carries the **old buggy `setup_repos.sh`** (the `TASKS_FILE` path fix from #519 isn't on this branch), so a bare `setup_repos.sh` will fail at repo checkout. The workaround is a one-flag override — no need to merge main (which would risk an `agent.py` conflict with #520). Here's the full runbook.

## What you run (on the opencode+deepseek machine)

```bash
# 1. Get the exact PR branch
cd <spelunk-oss>
git fetch origin && git checkout task/engineer-oss84-20260704-1548   # or: gh pr checkout 518

export DEEPSEEK_API_KEY=sk-...

# 2. Check out ONE task's repo (‑‑tasks-file works around this branch's setup_repos bug)
bash bench/setup_repos.sh --tasks 1 --tasks-file bench/agents/tasks_50.json

# 3. Same task, same seed, baseline condition — vary only the harness.
#    No --eval: we're validating the run + provenance JSON, not resolve_rate
#    (so no Docker/SWE-bench images needed).
bash bench/agents/swebench_run.sh --condition baseline --harness none        --model deepseek-v4-flash --api-key "$DEEPSEEK_API_KEY" --tasks 1 --seed 42
bash bench/agents/swebench_run.sh --condition baseline --harness opencode     --model deepseek-v4-flash --api-key "$DEEPSEEK_API_KEY" --tasks 1 --seed 42
bash bench/agents/swebench_run.sh --condition baseline --harness claude-code  --model deepseek-v4-flash --api-key "$DEEPSEEK_API_KEY" --tasks 1 --seed 42
```

That's the exact matrix cell my box couldn't do — `none` and `opencode` never ran against a real model here, and `claude-code` only ran on ambient Anthropic creds, not DeepSeek's Anthropic-compat endpoint.

## What I need back

1. **The three result JSONs** from `bench/results/` (paste or send):
   - `swebench-baseline-<ts>.json` (none)
   - `swebench-baseline-opencode-<ts>.json`
   - `swebench-baseline-claude-code-<ts>.json`
2. **Confirmation the patches are non-empty** — each JSON's `patch_file` should be set; a quick `wc -l` on the three `.patch` files under `bench/patches/…` is enough.
3. **`opencode --version` and `claude --version`** — so I can sanity-check they landed in the `harness_version` field.
4. **Any stderr** from a cell that fails or looks off.

## What "pass" means (what I'll verify in those JSONs)

Each row should show: correct `harness`; `endpoint_kind` = `native` (none/opencode) / `anthropic-compat` (claude-code); non-null `harness_version` for opencode + claude-code; non-zero `turns`/`input_tokens`/`output_tokens`; `patch_file` set; `harness_error: null` and `harness_exit_code: 0`. That satisfies oss^84's acceptance ("one task end-to-end under all three harnesses, each producing a patch + provenance JSON").

## Two likely snags

- **opencode:** the script writes its own scratch `opencode.json` provider (`spelunk-bench-deepseek`) into the task repo and runs headless with `--auto`. If your global opencode auth interferes, the per-repo provider should still win; if turns/tokens come back `0` on an otherwise-clean run, opencode may have renamed its `--format json` event fields (README flags this) — send me the raw output and I'll adjust the parser.
- **claude-code:** the script points `claude` at `https://api.deepseek.com/anthropic` via `ANTHROPIC_AUTH_TOKEN` in the subprocess only. If DeepSeek's compat endpoint produces malformed tool-call turns, rerun that one cell with `--endpoint-kind shim --shim-base-url http://<your-litellm>:<port>` (you'd need a LiteLLM proxy running) — but try the direct path first.

The optional **endpoint-fidelity check** (native vs anthropic-compat on the same task) is documented in `bench/agents/README.md` under "Endpoint-fidelity sanity check" — if you run it, that write-up closes the last placeholder in the README, but it's not required for the acceptance.

Want me to also tee up resolving the #518-vs-main staleness (it's mergeable/clean per GitHub, but 30 behind) so it's clean when you merge after the test?