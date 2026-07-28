# ADR-062: Temporal / as-of semantic search (do not revive)

**Date:** 2026-07-09
**Deciders:** founder (Johan), architect

> Decision: **drop permanently.** This ADR evaluates reviving temporal semantic
> search and recommends against building it as a standing subsystem. The
> sub-decisions below record the shape it *would* take, so a future reversal has
> a starting point rather than a blank page.

## Context

`spelunk search --as-of <sha>` shipped a storage layer for "semantically search
the codebase as it was at commit N" but was never wired to indexing. Nothing
ever called `create_snapshot` / `insert_snapshot_*`, so `list_snapshots()` was
always empty and every `--as-of` invocation errored. A later change removed the
surface outright (commit `633276682`): it deleted `storage/snapshots.rs`, dropped
the `--as-of` flag, and added migration `021_drop_snapshots.sql` to remove the
dead `snapshots` / `snapshot_files` / `snapshot_chunks` / `snapshot_embeddings`
tables. The original implementation (`69f070fc8`, migrations 016/017) is retained
in git history.

The task that spawned this ADR asks us to confirm the feature is
worth its cost **before** rebuilding it. This is that confirmation. Four
questions decide it: what triggers a snapshot, how snapshots are reclaimed, what
net-new value they add over `git show`, and whether the whole thing fits the
scope boundaries in [ADR-001](001-scope-boundaries.md).

### What a snapshot actually costs

The original design took a snapshot by resolving a ref, adding a detached git
worktree, and running a **full index of the historical tree** into parallel
per-snapshot tables: every file re-parsed, every chunk re-embedded, every chunk's
content and 896-dim vector stored again. A snapshot is therefore a complete
second copy of the index at a point in time, not a diff.

Two costs follow, and both recur per snapshot:

- **Storage.** `snapshot_chunks` duplicates chunk text; `snapshot_embeddings`
  duplicates the vector set. A snapshot is roughly the size of the live index.
  Ten snapshots is ten live indexes on disk, with no dedup across near-identical
  revisions.
- **Compute.** Embedding is GPU-bound and the forward pass dominates indexing
  time. A snapshot re-embeds the entire historical tree from scratch, so each
  snapshot pays the full index cost again. Snapshotting on every `spelunk index`
  would multiply steady-state indexing time by a large constant.

## Decision

Do not revive the snapshot subsystem. The four sub-decisions are resolved as
follows, in order of how much they drove the outcome.

### D3 – value vs `git show` (the deciding question)

Exact-SHA retrieval already exists without spelunk. `git show <sha>:path`
returns any file at any commit; `git log -S` / `git log -G` (pickaxe) find where
a string or pattern entered or left history. The **only** net-new capability a
snapshot adds is *semantic* search over historical state: "find the code that did
X, as it was three releases back," by concept rather than by known symbol or
path.

That capability is real but narrow, and it is already reachable on demand with
existing primitives: checking out a historical ref into a worktree and indexing
it produces exactly this, at zero standing cost, only when someone actually needs
it. The rare genuine need does not justify a permanent subsystem that pays
storage and re-embed cost continuously for a query almost no session runs.

Crucially, temporal *code* search sits outside spelunk's sharpened thesis.
spelunk's differentiator is remembering **why** a codebase is the way it is:
decisions, requirements, and superseded history live in the memory layer, which
already has temporal semantics (`memory --as-of <date>`, `timeline`,
`supersede`). "What the code looked like at commit N" is squarely git's job, not
a retrieval-engine gap. The valuable temporal axis (why) is already owned by a
cheaper, better-fit subsystem; the temporal axis a snapshot adds (what) is the
one git already serves.

**Recommendation: not worth the cost.**

### D4 – scope fit vs ADR-001

- **Boundary 4 (semantic search, complement grep):** nominal fit. Semantic
  search over history is something grep and `git show` cannot do. This is the
  strongest point in the feature's favour, and on its own it is not enough.
- **Boundary 2 (SQLite is the storage layer):** strained. The mechanism fits
  (parallel tables, sqlite-vec), but unbounded per-index duplication pushes a
  single-file local store toward multi-index bloat, which is the operational
  complexity that boundary exists to avoid.
- **Product thesis:** poor fit, per D3. The temporal need spelunk is
  differentiated on is already served by the memory layer.

**Recommendation: passes one boundary on a technicality, fails the thesis.**

### D1 – snapshot trigger (only if ever revived)

Snapshotting on every `spelunk index` is rejected regardless of the top-line
decision: it multiplies steady-state index cost and grows the database without
bound for a feature most runs never query. If the subsystem were ever revived it
must be **opt-in** and explicit, via a dedicated `spelunk snapshot <ref>` command
(or `index --snapshot`), so the cost is only paid when a user deliberately asks
to freeze a revision.

**Recommendation (conditional): opt-in only, never automatic.**

### D2 – retention / GC (only if ever revived)

The original layer had `delete_snapshot` but nothing ever called it, so
snapshots would have accumulated forever. Any revival must ship a retention
policy in the same change, not as a follow-up: a **keep-last-N** default with
**tag-pinned** exemptions (a user can pin a named snapshot so GC skips it), and
GC enforced at snapshot-create time. Shipping the create path without the reclaim
path is the exact footgun that left the original layer with an uncallable
`delete_snapshot`.

**Recommendation (conditional): keep-last-N + tag-pinned, GC enforced at create,
shipped atomically with the feature.**

### D5 – recommendation

**DROP, permanently, as a standing subsystem.** The net-new value (semantic
search over historical code) is narrow and speculative; the cost (a full
duplicate chunk-and-vector set plus a full GPU-bound re-embed per snapshot) is
concrete and recurring; the temporal need spelunk is actually differentiated on
is already owned by the memory layer at far lower cost; and the rare genuine case
is already satisfiable ad hoc by indexing a checked-out worktree, with no new
command surface, no retention policy, and no standing storage. Reviving a
permanent snapshot subsystem with its own trigger UX and GC policy is a poor
trade against every one of those.

## Non-goals

- **No implementation.** No code is written by this ADR. The removal in
  commit `633276682` stands; migration `021_drop_snapshots.sql` is not reverted.
- **No change to memory temporal features.** `memory --as-of <date>`,
  `timeline`, and `supersede` are unaffected. They are the endorsed temporal
  surface and remain the place the "history" need is served.
- **No shipped historical-search command.** This ADR does not add, document, or
  endorse a user-facing recipe for ad-hoc historical indexing. The observation
  that the capability is reachable on demand is reasoning for the drop, not a
  feature proposal.

## Consequences

- The `--as-of` surface stays removed. The task that spawned this ADR closes as
  `wont_do` once this ADR clears review; no scoped implementation task follows.
- The temporal story for spelunk is memory-layer only: decisions and their
  supersession over time, not code-state over time.
- **Revisit if:** concrete, repeated user demand appears for semantic search over
  historical code state that the memory layer and ad-hoc indexing genuinely
  cannot serve. A reversal would build on the D1/D2 shape above and would need to
  solve cross-revision embedding dedup to be affordable, since full-copy
  snapshots do not scale past a handful of revisions.
