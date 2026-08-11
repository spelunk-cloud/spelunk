# Flash harness matrix: paired statistics (§6)

Generated via `bench/paired_stats.py`, n=3 seeds per cell, 49-task slice.


## harness=none: baseline vs spelunk_search

```
## Paired comparison (plan §6)

Baseline  cell: model=deepseek-v4-flash (api) | harness=swebench-verified | condition=baseline | filter=harness=none | n=49
Condition cell: model=deepseek-v4-flash (api) | harness=swebench-verified | condition=spelunk_search | filter=harness=none | n=49

### Pass rate (bootstrap 95% CI over per-seed means)
  baseline : 0.612 +/- 0.020  [0.592, 0.633]
  condition: 0.646 +/- 0.010  [0.633, 0.653]
  delta    : +0.034

### McNemar exact test (paired by task_id)
  paired tasks : 49
  both pass    : 31
  both fail    : 17
  condition only (gains)     : 1
  baseline only (regressions): 0
  discordant   : 1
  exact p      : 1.0000
  result       : not significant

Power note: a 50-task slice only detects large effects (~+/-15pp). Headline claims require the filtered subset or the 150+ question set.

```


## harness=none: baseline vs spelunk_full

```
## Paired comparison (plan §6)

Baseline  cell: model=deepseek-v4-flash (api) | harness=swebench-verified | condition=baseline | filter=harness=none | n=49
Condition cell: model=deepseek-v4-flash (api) | harness=swebench-verified | condition=spelunk_full | filter=harness=none | n=49

### Pass rate (bootstrap 95% CI over per-seed means)
  baseline : 0.612 +/- 0.020  [0.592, 0.633]
  condition: 0.878 +/- 0.031  [0.837, 0.898]
  delta    : +0.265

### McNemar exact test (paired by task_id)
  paired tasks : 49
  both pass    : 29
  both fail    : 3
  condition only (gains)     : 15
  baseline only (regressions): 2
  discordant   : 17
  exact p      : 0.0023
  result       : SIGNIFICANT (p<0.05)

Power note: a 50-task slice only detects large effects (~+/-15pp). Headline claims require the filtered subset or the 150+ question set.

```


## harness=opencode: baseline vs spelunk_search

```
## Paired comparison (plan §6)

Baseline  cell: model=deepseek-v4-flash (api) | harness=swebench-verified | condition=baseline | filter=harness=opencode | n=49
Condition cell: model=deepseek-v4-flash (api) | harness=swebench-verified | condition=spelunk_search | filter=harness=opencode | n=49

### Pass rate (bootstrap 95% CI over per-seed means)
  baseline : 0.952 +/- 0.010  [0.939, 0.959]
  condition: 0.973 +/- 0.010  [0.959, 0.980]
  delta    : +0.021

### McNemar exact test (paired by task_id)
  paired tasks : 49
  both pass    : 47
  both fail    : 0
  condition only (gains)     : 1
  baseline only (regressions): 1
  discordant   : 2
  exact p      : 1.0000
  result       : not significant

Power note: a 50-task slice only detects large effects (~+/-15pp). Headline claims require the filtered subset or the 150+ question set.

```


## harness=opencode: baseline vs spelunk_full

```
## Paired comparison (plan §6)

Baseline  cell: model=deepseek-v4-flash (api) | harness=swebench-verified | condition=baseline | filter=harness=opencode | n=49
Condition cell: model=deepseek-v4-flash (api) | harness=swebench-verified | condition=spelunk_full | filter=harness=opencode | n=49

### Pass rate (bootstrap 95% CI over per-seed means)
  baseline : 0.952 +/- 0.010  [0.939, 0.959]
  condition: 0.946 +/- 0.031  [0.918, 0.980]
  delta    : -0.007

### McNemar exact test (paired by task_id)
  paired tasks : 49
  both pass    : 45
  both fail    : 0
  condition only (gains)     : 1
  baseline only (regressions): 3
  discordant   : 4
  exact p      : 0.6250
  result       : not significant

Power note: a 50-task slice only detects large effects (~+/-15pp). Headline claims require the filtered subset or the 150+ question set.

```
