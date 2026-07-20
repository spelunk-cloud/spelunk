# ADR-070: `init` embed lifecycle, and the search warmup contract

**Date:** 2026-07-15
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** operates strictly inside the existing embedding
split (the server computes vectors, the CLI is the only persistent store for
index data) and does not reopen it. Extends
[ADR-068](068-zero-setup-onboarding-git-notes-memory-fallback.md)'s honesty
posture (a surface that cannot answer says so, and says what would make it
answer) from the pre-`init` memory surface to the post-`init` search surface.

## Context

A fresh `spelunk init` on a large real-world checkout, on a machine with **no**
`spelunk-server` already running, produces an index with **zero** embeddings and
a semantic search that answers `No results found.` for every query, forever,
until the user happens to re-run `spelunk index` by hand. Nothing in the output
says the index is unusable. The recorded reproduction, on a mid-size repo of
roughly 8.2k files and 27.7k chunks:

```
Index: 8226 files, 27734 chunks, 0 embeddings
Server: http://127.0.0.1:7777  ✓  (auto-started)
```

and then, five minutes later, a server resident at roughly 800MB doing 0% CPU
and 0% GPU, with `spelunk status` reporting `Embeddings: 0`.

This is the worst class of defect the product can have. The tool is not slow and
it is not erroring. It is confidently reporting the absence of evidence over a
corpus it never looked at.

### The mechanism, traced in code

Four independent facts compose into the failure. Each was verified against the
tree, and two of them are not what the intake ticket recorded.

1. **`init` embeds before it starts the server.** The index/embed step runs
   first, and the server auto-start runs after it. On a fresh machine the embed
   step therefore probes for a server that this very command is about to start.

2. **A not-ready embedder is treated as terminal.** The embed step is gated on
   `let embed_ready = matches!(tier.caps(), Some(c) if c.index_embed);` and
   `if tier.is_server() && embed_ready`. When the probe fails, the phase is
   skipped and a notice is printed. The chunks are already durably stored, so
   nothing is lost, but nothing schedules the work either.

3. **The server cannot rescue this, by design.** It is stateless with respect to
   index data. `POST /v1/projects/{id}/index/embed` takes chunk texts in the
   request body and returns vectors; the handler's own contract says the server
   does not store them. The server has no access to the project's SQLite
   database, no notion of which chunks lack embeddings, and no project path. A
   server-side drain loop is not missing, it is **not expressible** without
   inverting that split. The idle server is correct behaviour, not a bug.

4. **Search then declines to fall back.** In `auto` mode the fallback to
   ast-grep on an empty result set is gated on `index_is_stale(&db_path)`, which
   samples file hashes for drift. A just-built index is not stale, so the guard
   is false, so no fallback fires, so `No results found.` prints.

### Two corrections to the record

Both of these were carried as settled premises into this decision and are wrong.
They are recorded rather than quietly fixed, because in each case acting on the
stated premise produces a change that ships green and fixes nothing.

> **The empty-index guard is not what suppressed the fallback.** The intake
> ticket's root cause reads "chunk_count≠0 so the empty-index fallback doesn't
> fire". There *is* a real `chunk_count == 0` fast path in `search`, and it is
> not the guard that failed here. The guard that should have fired and did not
> is the staleness check on the empty-result path. An engineer sent to fix "the
> `chunk_count` guard" edits a correct line and leaves the defect standing.

> **Detaching the embed is inert on its own.** The detach proposal's headline
> item is that `init` hardcodes `detach_embed: false` and that flipping it is
> "nearly a one-line change". It is one line, and on the reported bug it is a
> **no-op**. `args.detach_embed` is evaluated *inside* the
> `if tier.is_server() && embed_ready` branch. On a fresh install that branch is
> not taken, so the flag is never read, so the embed is skipped exactly as it is
> today. The change demos green on any machine that already has a warm server,
> which is every developer machine that has run the tool once. This is the
> single highest-risk item in this cluster: a fix that passes review, passes a
> hand-check, and does not touch the bug.

## Decision

### D1 – The framing is a false dichotomy. The reorder and the detach are one change.

The intake ticket frames an architect call between (a) starting the server
before the embed phase so a fresh `init` embeds inline, at the cost of restoring
a very long foreground wait, and (b) wiring a detached background embed instead
of silently skipping.

Neither is a choice, because neither works alone:

- **(a) alone** buys a correct index for the price of holding the terminal for
  the entire embed pass. On the measured repo that is **102.9 minutes** of embed
  inside a 104.2-minute `init`: over an hour and three quarters (see D6). That is
  not a fix, it is a different complaint.
- **(b) alone** is the no-op above. The detach lives behind the readiness gate
  that the missing server fails.

The decision is **both, as a single indivisible change**: start the server
before the embed phase *and* hand the embed pass to the detached worker. The
reorder is what makes the detach reachable; the detach is what keeps the reorder
from becoming a foreground wall. Shipping either half alone is a regression or a
placebo, so they do not get separate reviews.

### D2 – A not-ready embedder is a transient condition to wait on, not a terminal condition to skip.

The reorder alone does not fix the cold-start case, and this is the part that no
ticket in the cluster records.

`ensure_server_running` waits for **liveness**, not model readiness. Its own
comment is explicit: health goes live at socket bind, deliberately, *before* the
model is loaded. The model then loads on a background task and flips the
embedder slot to `ready` some time later. So after the reorder, a genuinely
fresh machine still arrives at the embed step with the embedder in `loading`,
still computes `embed_ready == false`, and still skips. The reorder converts
"no server" into "server warming" and produces the identical zero-embedding
index.

Therefore: **the embed worker owns the wait.** It polls the embedder state with
a bounded backoff and proceeds when the state reaches `ready`. Only `unavailable`
and `disabled` are terminal, and each already has a distinct, actionable notice
that must be preserved. `loading` is never a reason to abandon durable queued
work.

This needs no new persistence. The enabling fact, verified: the queue is already
durable and resumable. `chunks_missing_embeddings` reconstructs it with a
`LEFT JOIN ... WHERE e.chunk_id IS NULL`, and the detached entry point rebuilds
the queue from the database rather than carrying it across the process boundary.
The recovery path has always worked. It was only ever undiscoverable.

Resume granularity is **per batch, not per chunk**. `insert_embeddings` writes
each batch's rows in a single transaction (matching the `update_graph_ranks`
batch pattern), so a batch either lands whole or not at all. Each row within
that transaction is written via an atomic delete-then-insert, not a bare
`INSERT OR REPLACE` — the `embeddings` table is a `vec0` virtual table, which
does not honour that conflict clause, so a repeated `chunk_id` (within or
across batches) genuinely replaces the existing row instead of raising a
UNIQUE-constraint error. The queue-durability argument holds unchanged: a
worker killed mid-batch keeps every batch it had already committed, the
interrupted batch commits nothing, and the `LEFT JOIN` re-queues exactly that
batch on the next run, with no duplicate row (the insert is keyed on
`chunk_id`) and no silent gap. The trade this buys is bounded: the worst case
on an untimely kill is recomputing one batch's embeddings, capped by the
calibrated batch size (at most 256 chunks, and usually the smaller
duration-calibrated size rather than the ceiling), not unbounded work. This
supersedes an earlier per-chunk autocommit whose per-row transaction commit
bought finer resumability than this ADR requires; batching the commits trades
that surplus granularity for throughput, a trade this decision anticipated and
pre-accepted rather than one discovered after the fact. The overhead removed is
the per-row commit cost (a WAL fsync per autocommit under the default
`synchronous=FULL`); its magnitude is hardware-dependent and small relative to
the GPU-bound embed phase, so the change is a modest throughput refinement, not
a headline speedup.

### D3 – Search never reports an absence it cannot substantiate.

This is the invariant the cluster exists to protect, and it is stated once, in
one place, so that no future gate can quietly fail open again:

> **`No results found.` is only ever printed when the corpus that was searched
> was complete.** Whenever coverage is partial, the output names what was
> incomplete. Silence is not an available option.

Embedding coverage becomes a first-class input to `search`, replacing the
staleness proxy on the empty-result path. Three states, exhaustively:

| Coverage | `auto` mode | Explicit `semantic` / `hybrid` |
| --- | --- | --- |
| `0` | Fall back to text/ast-grep, with a notice naming warmup as the reason | Actionable error naming warmup and the resume command. Never `No results found.` |
| `0 < c < 100` | Run KNN; **always** emit a one-line warmup notice carrying the coverage percentage and its shape (below). If the result set is empty, fall back to text rather than print `No results found.` | Run KNN; emit the same warmup notice so a thin result set is never mistaken for a complete one |
| `100` | Today's behaviour, no notice | Today's behaviour, no notice |

**Coverage is measured in chunks**, and that is the right unit for it: it answers
"what fraction of the corpus can KNN see", and a chunk that is embedded really is
searchable. This is deliberately *not* the unit D4 uses to report progress, for
reasons D4 and D6 develop. The two are different questions and this ADR keeps
them apart everywhere.

Partial coverage is safe to serve, and this is load-bearing rather than
optimistic: `search_similar` is a pure KNN over whatever rows exist in the
`embeddings` table, with no completeness gate at the storage layer, and KNN is
order-independent. Any prefix of the queue is immediately useful. That is what
makes progressive availability nearly free, and it is why the honest thing to do
with a partial index is to serve it and label it, rather than withhold it.

**But the prefix has a shape, and the notice must not let a reader assume the
wrong one.** Because the queue is `ORDER BY c.id` and ids follow the indexer's
walk, a prefix is *the first N files*, not a sample spread across the repo. At 40%
coverage the user does not have a thinner picture of the whole codebase; they have
a complete picture of some of it and a **systematic blind spot** over the rest.
Those two failure modes want different reactions, and a bare percentage reads as
the first. So the notice names the shape, not just the number: coverage is
described as partial *and* front-loaded by indexing order, so a user who gets no
hit for a subsystem knows that "not embedded yet" is a live explanation rather
than a remote one. This is the D3 invariant applied to itself. An honest
percentage attached to a misleading mental model still leaves the user reasoning
from an absence the tool cannot substantiate.

**No coverage threshold is introduced.** A threshold is a tuning knob that
invites bikeshedding and, worse, encodes a claim ("90% is basically complete")
that the tool cannot substantiate for any particular query. "Incomplete" is a
fact; the percentage is reported and the user judges. Routing stays as it is,
since hybrid already blends.

All notices go to **stderr**, matching the existing skip notices, so `--format
json` and `jsonl` output stay machine-clean.

### D4 – `status` knows about its own background job. It does not guess.

Today `status` prints, at `0/27734` against a demonstrably idle server:

```
Embedding in progress  0/27734 embedded (27734 pending) (a background embed may be running; re-run `spelunk index` to resume if not)
```

Both halves of that line are defects. It asserts "in progress" from a pure
function of two integers, which cannot distinguish a running job from an
abandoned one, and then hedges the assertion away in the same breath. A tool
guessing about its own subprocess is worse than one that stays quiet, because
the guess is what stops the user from investigating.

The detached worker records its liveness, and `status` reads it. There is
already a correct precedent in-tree to copy rather than reinvent: the server's
own pid / port / db-path state files, its `pid_is_alive` check, and its
classification of a recorded-but-unresponsive daemon into healthy, hung-ours,
and foreign (pid reused by an unrelated process). The worker reuses that shape,
including the foreign-pid case.

This is **liveness**, which is a distinct concern from **diagnostics**. The
detached children's stdout and stderr are separately being routed to an
`index-background.log` beside the index, with a pointer printed after the status
line (#624). That work stands on its own and D4 does not duplicate it: a log
says what the worker *said*, a pid says whether it is *there*, and `status`
needs the second to stop guessing. D4 builds on that change rather than
re-plumbing the spawn, and the two must not land as competing edits to the same
spawn path.

`status` then reports what it knows:

- worker alive → `Embedding in progress`, with the two measures below
- no worker, pending > 0 → `Embedding incomplete`, with the same two measures,
  plus the resume command. Not "in progress".
- embedder `unavailable` → say so, and point at the server logs.

The hedging parenthetical is deleted, not reworded.

#### Coverage and progress are two measures, in two units, under two names.

This is the part of D4 that is easiest to get wrong, because the wrong version is
what the line already prints and it looks correct.

`N/M (x%)` embedded-chunks-over-total-chunks is **the same numerator shape that
produces the estimator defect in D6**, and it is wrong for the same reason. The
queue runs in `chunks.id` order and mean chunk size grows roughly 4x through it
(137 → 577 tokens across the id range on the profiled repo), so a chunk fraction
early in the run is not a work fraction:

| measured at | chunks done | work done |
| --- | --- | --- |
| sample point | 42.6% | **21.2%** |

So `Embedding in progress 11813/27734 (42%)` beside an ETA is a **true statement
about coverage and a false one about progress**. The user infers most of the wait
is behind them when **79% of it is ahead**. That is D6's own defect re-entering
through a different line, and fixing D6's estimator while leaving this one alone
fixes half of it.

The decision:

- **Coverage stays chunk-shaped** (D3). 42% of chunks really are searchable, so
  the number is not wrong; it is only wrong as an answer to a question nobody
  asked it.
- **Progress and the ETA become token-weighted**, over `chunks.token_count`,
  which already exists (added by migration `008_token_counts.sql`, written on
  chunk insert, with a backfill path for pre-existing indexes). Note that
  `chunks_missing_embeddings` does not currently project that column; it will
  need to, or the denominator needs its own aggregate.
- **The two never share a name, and no percentage is ever printed bare.** Every
  percentage names its denominator, so the two can be seen to be different rather
  than looking like a disagreement:

```
Embedding in progress   searchable 11813/27734 chunks (42%)  ·  21% of work done, ~54 min left
```

`searchable` answers "what can search see"; `of work done` answers "how much of
the wait is behind me". They are *supposed* to diverge, and on a real repo they
diverge by 2x. Two numbers under one label reads as a bug; two numbers under two
labels reads as the fact it is.

`token_count` is good enough for this, and the honest caveats are that it stores
`estimate_tokens(content)` (`chars/4`, not a tokenizer count) and carries roughly
±25% per-chunk error (direct tokenization of all 27,734 chunks: 91.9% within
±25%, p10/p50/p90 = 0.774 / 0.912 / 1.086). Neither sinks it, and the comparison
is what matters rather than the absolute accuracy: **that error is unbiased and
averages out over tens of thousands of chunks, whereas chunk-counting is wrong by
a systematic 4.4x in a consistent direction across the run.** The choice is not
between an estimate and the truth, it is between an unbiased estimate and a
biased one.

### D5 – Queue ordering lands after, and stays decoupled.

Prioritising the embed queue (recency first on a cold index, graph rank first on
a warm one, replacing today's `ORDER BY c.id`) is correct and is **not** part of
this change. It is not required for correctness: ordering only becomes
observable once a partial index is reachable at all, which is what D1 through D4
deliver. It changes the same query the worker consumes, so landing it
concurrently buys two contended edits in one file for no user-visible benefit
during the window when the index is still unreachable.

Measurement sharpens *why* it is worth doing later, and narrows what it may
claim. **Reordering does not reduce total work, and must not be justified as if
it did**: measured tokens/s is roughly flat across the run (1.4x variance,
against 4.4x for chunks/s), so the queue costs what it costs regardless of the
order it is drained in. Its entire value is **which chunks land first**: in id
order a partial index is an arbitrary alphabetical slice, in graph-rank order it
is the most important code first. That is a claim about the D3/D4 surface, which
is precisely why it sequences after D1 through D4 rather than beside them: before
those land there is no partial index for an ordering to be observable in, and
afterwards the value is immediate and needs no throughput argument to carry it.

One constraint on it, so this decision does not foreclose a live one: the
ordering key is computed from file metadata and graph rank, both of which are
independent of chunk boundaries. Chunk granularity is under separate active
investigation, and an ordering keyed on anything chunk-shaped would have to be
redone when that lands.

### D6 – The measured cost, and what it means for the ETA.

The figure attached to this cluster is wrong in both of the ways a number can be
wrong, and the corrected version changes what the work is.

> **The embed phase is not "~28 min".** The ETA-derived "~28 min" that titles the
> throughput ticket was never a measurement. It is what the progress estimator
> *predicted*, sampled early in the run. The measured wall clock for `init` on
> the same repo (8,226 files / 27,734 chunks) is **6,250.65s = 104.2 minutes**:
> embed **6,176.07s (98.81%)**, parse 73.31s (1.17%), graph rank and conventions
> 1.27s (0.02%). The estimator under-reported by **3.2x**: sampled at 42.6% of
> chunks, only 21.2% of tokens were done, giving a 16.8-minute chunk-derived ETA
> against a 54.3-minute token-derived one. The corrected number must be used
> everywhere; "28 minutes" should not be repeated.

> **The docs framing on that ticket was already withdrawn and does not survive.**
> Its title still reads "contradicts the five minutes doc promise". The founder
> corrected this on the ticket itself: the getting-started doc's "first five
> minutes" scopes to the sections *before* indexing, and introduces `init`
> afterwards as a separate later step. The doc never claimed indexing is fast,
> so there is no contradiction to fix. The ticket was explicitly downgraded at
> that time. Reviving the "broken promise" framing on the strength of the larger
> number would re-file a complaint the founder has already rejected on the
> merits, and the larger number does not revive it, because the promise never
> covered this step.

What genuinely remains from that ticket is narrower, and it is a consequence of
D1 rather than an independent item: **the progress estimator itself
under-reports.** Once the embed pass is detached, `status` is the only window
onto it, so an ETA that is wrong by 3x stops being a cosmetic annoyance and
becomes the primary signal a user has. The estimator correction therefore folds
into the `status` work in D4. Any prose about realistic timings belongs to the
marketing site, not to this repo.

#### The root cause has a second consequence, and it is not cosmetic.

`per_entry` is a per-**chunk** rate, calibrated on early chunks and applied to a
queue whose per-chunk cost is **not stationary**: it grows ~4x through the id
order (D4). That single fact is not one bug in the ETA. `RateEstimate`'s own doc
comment names its consumers: *"`next_batch_size`, `batch_timeout`, and the
displayed ETA (`format_eta`) all read `per_entry()` from the same instance so
they can't disagree."* The design goal is sound and achieved. They do not
disagree. **They are consistently wrong together**, because they share one input
whose unit does not match the cost it is predicting.

The second face is measured. On a 581-chunk index the CLI sent **837 chunks
across 7 requests**; `837 − 581 = 256`, exactly one batch computed by the server
and then discarded. `RateEstimate` is an EMA, so the state that set batch 5's
deadline is the blend of every sample before it, not the latest sample.
Reconstructed from the request arrival times (the audit log fires on handler
entry, and the global concurrency cap of 256 means the retry never queued, so
each timestamp is a send time):

| batch | chunks | took | sample | `per_entry` (EMA) | `batch_timeout` |
| --- | --- | --- | --- | --- | --- |
| 1 | 1 | 0.054s | 54.0ms | 54.0ms | 60.0s (floored) |
| 2 | 4 | 0.219s | 54.8ms | 54.7ms | 60.0s (floored) |
| 3 | 32 | 5.717s | 178.7ms | 116.7ms | 119.5s |
| 4 | 256 | 6.795s | 26.5ms | **71.6ms** | **73.3s** |
| 5 | 256 | **≥73.076s** | **≥285.5ms** | - | - |

1. After four batches the EMA sits at `per_entry` = **71.6ms**. Batch 4's raw
   sample was 26.5ms, but the blend is dominated by batch 3's slow 178.7ms
   sample.
2. `next_batch_size` divided the 240s target by 71.6ms, asked for 3,352 chunks,
   far more than the `MAX_BATCH = 256` ceiling, and got 256.
3. `batch_timeout` = `TIMEOUT_SAFETY_FACTOR (4) × 0.0716 × 256` = **73.3s**,
   comfortably inside the `[60s, 1800s]` clamp range: **no clamping occurred,
   and the `MIN_REQUEST_TIMEOUT` floor never engaged.**
4. Batch 5 was 256 chunks at **7.4x the size** (192 to 1,415 chars/chunk, as
   the queue crosses from tiny chunks into 120-line windowed ones), and its
   real cost was **≥285.5ms/chunk**. At 73.3s the client abandoned it, halved
   to 128 and retried. The server never learns a client gave up: it finished
   all ~73s of GPU work and threw the result away.

The predicted 73.3s deadline matches the observed 73.076s gap between the
abandoning request and its retry to **0.3%**, which is strong evidence this
reconstruction is the actual mechanism and not a story fitted after the fact.

**~73s of a 198.2s run is wasted GPU: 37%.** Reproduced on an idle box, so it is
not contention. Note which guard saved it: **none did.** The 4x safety factor
did not, because batch 5's real cost was a **4.0x** under-estimate
(285.5 / 71.6) against a 4x margin: the margin was consumed exactly, leaving
zero. The floor did nothing; it was never reached. Nothing bounded the damage.
The deadline landed essentially on the batch's own completion time, which is
the *maximal-waste* case: the client waits the full duration and then throws
the result away anyway. And the sizing consumer is not an innocent bystander
here: the same rate selected an oversized batch *and* set the deadline that
batch's own cost would consume to the wire.

That the server computes an abandoned batch to completion regardless is an
**independent fault (no request cancellation on client disconnect) with an
independent fix**: a token-weighted rate makes spurious timeouts rarer, not
impossible, so this is out of scope here and tracked as issue #631.

So the estimator defect and the timeout defect are **one defect with two faces**,
and a token-weighted rate addresses both. This is why the fix belongs in D6's
fold into D4 rather than in a separate throughput ticket: an engineer sent to fix
"the ETA" would correct the user-visible number and leave a timeout running on
the same broken input, still burning a batch of GPU per size transition. The
estimator is not only the user's window onto a detached pass; it is also feeding
a deadline whose entire safety margin had been consumed by the rate error. The
exposed case is scoped by the data: **abrupt per-batch cost transitions, not
repo size.** The large repo behind this cluster ran its full 102.9-minute embed
with zero retries, because its per-batch cost changes gradually across ~108
batches and the EMA tracks it; the small repo above jumps 7.4x in a single
batch. This is plausibly also the mechanism behind the connect-timeout reports
on large repos, which are tracked separately; if so, the trigger there was a
local transition in the queue rather than the repo's scale, which is a testable
prediction. That link is offered as a lead, not as an established diagnosis.

#### One boundary the fix must respect: ratios cancel the bias, budgets do not.

`estimate_tokens` is `chars/4`, and its bias is **corpus-dependent, not
constant**: aggregate `real/est` measures **0.930** on a Ruby/TS repo and
**1.387** on an MDX/TS repo, so the same estimator over-counts one corpus by 7%
and under-counts another by 39%. It is a **scale factor**, which is what makes it
tractable, and it fixes what the fix is allowed to look like:

- It **cancels in a ratio** (tokens done / tokens total). D4's progress
  percentage is safe by construction.
- It **cancels in a rate that is calibrated in the same units it predicts in**.
  A `per_token` measured this run as `elapsed / estimated tokens in the batch`
  absorbs the corpus's bias into the calibration constant, and `batch_timeout`
  built from it is then predicting estimated-tokens against a rate per
  estimated-token. The bias appears on both sides and drops out.
- It **does not cancel where an estimated token count meets a constant expressed
  in true tokens.** `batch_timeout` is exactly this kind of absolute-budget
  consumer, so the constraint is: the rate stays **measured, per-run, and
  continuously recalibrated**. A hardcoded tokens/sec throughput constant is not
  a valid substitute, and **a rate cached across runs or reused across repos is
  a defect**, not an optimisation to warm the ETA up faster. A 1.49x swing in
  bias between two real corpora is a foreseeable 1.49x error in an absolute
  deadline. Keeping the EMA also matters for the same reason within a single run,
  since a repo whose file types are unevenly distributed through the walk has a
  bias that drifts as the walk proceeds.

The existing `RateEstimate` shape already satisfies all of this. The change is to
its **unit**, not to its architecture: one authoritative rate, calibrated from
observation, read by every consumer. That was the right design and it stays.

## Consequences

- A fresh `init` returns the prompt after parsing, with embeddings arriving in
  the background, and every surface that can observe partial coverage says so.
- The pathological case is inverted: the tool's least-informed moment is now its
  most talkative, rather than its most confident.
- `init` gains a dependency on the server having been started first. The
  auto-start is TTY-gated, so non-interactive `init` (CI, hooks) still legitimately
  produces an unembedded index. That path is now **required** to state it, under
  D3, rather than being distinguishable only by reading the chunk counts.
- Users on a warm server see a behaviour change: `init` no longer blocks through
  the embed pass. The opt-out for scripted use is retained.
- The notice during warmup will be seen often on large repos, for a long window.
  This is intended. The window is real, and the alternative to mentioning it is
  the defect this ADR exists to remove.
- **Two percentages now exist where users saw one, and they will disagree by 2x
  on a real repo.** That is the intended outcome, not a rough edge to sand down:
  coverage and progress are different questions with different answers, and the
  single number that used to serve both was the defect. The cost is that both
  numbers must always be labelled, everywhere, including anywhere a future
  surface reports embedding state. An unlabelled percentage is a regression to
  the behaviour D4 removes.
- Making the rate token-weighted changes `batch_timeout` and `next_batch_size`
  as a side effect, because they read the same rate. This is intended and is the
  point of D6, but it means the change touches request sizing and deadlines, not
  only a displayed string, and should be tested as such. The expected direction
  is fewer abandoned batches; the 37% wasted-GPU case above is the specific thing
  that should stop reproducing.

## Non-goals

- **Chunk granularity.** Under separate active investigation, with an unmeasured
  recall cost and a throughput win **still being measured**. The throughput half
  is currently *modelled*, not measured, and the model assumed a chunker we do
  not have: it assumed oversized chunks split into pieces at or under the token
  cap, whereas the re-window path is **line-based** (120 lines, 15 overlap)
  regardless of that cap, so lowering the cap constant alone would not produce
  chunks at the cap. The number will move. This is recorded so the sentence is
  not later quoted as settled; it changes no decision here, and D5 keeps the
  ordering key independent of chunk boundaries either way. Not decided here.
- **Embedder throughput, device selection, and GPU acceleration.** The embedder
  is ~99.8% GPU-bound (measured again here: GPU utilization 96-99%, mean 98.2%,
  during embed, against 0.06% of CLI CPU), so disaggregating it is a no-op. Not
  decided here.

  One trap worth recording, because the obvious experiment would come back green
  and mean nothing. **On any non-CPU device the sub-batch sizing constants are
  unreachable, not merely ineffective.** In `embed_batch` the device check
  precedes the length check in the same short-circuiting condition
  (`b == 1 || !matches!(self.device, Device::Cpu) || max_seq > BATCH_MAX_SEQ`),
  so Metal takes the sequential path unconditionally and `BATCH_MAX_SEQ` is never
  evaluated. Measured: 8 chunks in one request take 202.4ms, the same 8 as eight
  separate requests take 208.6ms, a ratio of **1.03**. The forward pass is not
  batched at all on that device, so the constants cannot matter, and **an A/B of
  them on a Mac would measure a no-op and prove nothing**. This scopes rather
  than contradicts [ADR-052](052-f2llm-cpu-batch-throughput.md), whose batching
  work is explicitly the `Device::Cpu` path, where these constants are live.
- **Cloud or remote embedding.** Trades a GPU-bound constraint for a metered
  network one with no wall-clock win.
- **Reopening the embedding split.** The server stays stateless with respect to index data.
  Every alternative that "just has the server finish the job" is a rewrite of
  the storage boundary, and none of them is needed: the CLI-side worker already
  has the database, the queue, and the resume path.

## Security implications

None material. The change reorders existing phases, extends an existing
subprocess spawn, and adds a pid-liveness file alongside the server's existing
state files in the same state directory, under the same permissions. No new
network surface, no new authentication path, and no new data leaves the machine.
The embed endpoint remains auth-gated and batch-capped as it is today. The
foreign-pid case in D4 is explicitly handled so a recycled pid cannot be
misreported as a live embed worker.
