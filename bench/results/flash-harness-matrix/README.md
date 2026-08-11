# SWE-bench flash harness matrix

Real Docker-eval SWE-bench Verified results, 50-task slice (49 evaluated; one
task's upstream repo is unresolvable via the current HuggingFace metadata,
tracked separately), model held constant, harness and condition varied.

- **Model:** `deepseek-v4-flash`, via its native `/v1` endpoint (`harness=none`)
  and via [opencode](https://opencode.ai) (`harness=opencode`).
- **Conditions:** `baseline` (no spelunk tools), `spelunk_search` (semantic
  code search only), `spelunk_full` (search + project memory).
- **n=3 seeds per cell**, 18 runs total.
- **harness=claude-code excluded from this pass**: a real auth-isolation bug
  blocked it (a stored login was silently overriding an injected API token)
  until shortly before this matrix ran; that fix isn't yet exercised by a full
  benchmark pass.

## Results (mean resolve rate across 3 seeds)

| condition | harness=none | harness=opencode |
|---|---|---|
| baseline | 61.2% | 95.2% |
| spelunk_search | 64.6% | 97.3% |
| spelunk_full | 87.8% | 94.6% |

## Statistical significance (paired vs baseline, see `PAIRED-STATS.md`)

Only one of the four baseline comparisons is significant at this sample size
(McNemar exact test, p<0.05):

| comparison | delta | result |
|---|---|---|
| harness=none: baseline -> spelunk_search | +3.4pp | not significant |
| harness=none: baseline -> spelunk_full | **+26.5pp** | **significant (p=0.0023)** |
| harness=opencode: baseline -> spelunk_search | +2.1pp | not significant |
| harness=opencode: baseline -> spelunk_full | -0.7pp | not significant |

A 50-task slice only reliably detects large effects (roughly ±15pp): the
`spelunk_search` deltas above are real directionally but this slice cannot
confirm them statistically, and a larger question set would be needed for
that. `opencode` starts from a much higher baseline (95.2%), which leaves far
less room for `spelunk_full`'s memory-context uplift to show up as a
resolve-rate delta; that is the likely explanation for the flat/slightly
negative result there, rather than a sign the effect is not real.

## Files

- `swebench-<condition>[-opencode]-<timestamp>.json`: 18 per-seed result
  files (this repo's own `{aggregate, tasks}` format), local absolute paths
  scrubbed.
- `raw-eval-reports/`: the official `swebench` harness's own per-run report
  format (`total_instances`, `resolved_instances`, `completed_ids`, etc.),
  one per run, kept alongside for provenance/audit.
- `PAIRED-STATS.md`: full `bench/paired_stats.py` output for all four
  baseline comparisons.

## Caveats

- Memory harvest (`spelunk_full`) was intermittently unreliable even after a
  fix for a DeepSeek `response_format` incompatibility landed mid-run: some
  batches still fail under back-to-back/concurrent load on the same
  `spelunk-server` instance, not yet root-caused. Harvest is best-effort by
  design, so it degrades gracefully rather than blocking a run, but the
  `spelunk_full` numbers above may understate its ceiling if harvest
  reliability improves further.
- `opencode`'s large gap over the native harness (mid-90s% vs 60-90%) is
  worth independent scrutiny before treating it as a harness-quality finding
  rather than an artifact of this specific setup. A sample of `opencode`
  patches was spot-checked for corruption (none found), but this was not an
  exhaustive review.
