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