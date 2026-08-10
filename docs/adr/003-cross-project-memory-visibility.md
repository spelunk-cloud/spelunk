# ADR-003: Cross-Project Memory Visibility for `spelunk context` / `spelunk memory search|list`

**Status:** Approved
**Date:** 2026-06-11
**Context:** 2026-06-11 SSE/local-server incident (spelunk-oss issue #375, PRs #379/#384, all closed invalid). Root cause analysis in memory `[[project_pr32_chain_verifier_retention]]`-adjacent decisions and `[[project_adr_approval_gate]]`.

## Problem

`spelunk link <path>` (and `spelunk unlink` / `spelunk links`) already build a
cross-project dependency graph in the global registry
(`~/.config/spelunk/registry.db`, `project_deps` table — see
`crates/spelunk-cli/src/cli/cmd/link.rs` and `links.rs`). `spelunk search`
already walks that graph: `search_all_dbs_linearrag()` in
`crates/spelunk-cli/src/cli/cmd/search.rs` opens the primary project's index
DB plus every linked dep's index DB, merges results, and annotates dep
results with `project_name` / `project_path`.

**Memory does not do this.** `spelunk context` (`crates/spelunk-cli/src/cli/cmd/context.rs`)
and `spelunk memory search|list` (`crates/spelunk-cli/src/cli/cmd/memory/search.rs`,
`memory/list.rs`) call `open_memory_backend(cfg, mem_path, ...)` once and query
only that single backend (local SQLite `memory.db`, git-notes, or a configured
`server_url`). The registry's `project_deps` graph is never consulted.

### Concrete failure mode (2026-06-11 incident)

- Decision #134 ("SSE memory stream → Cloud-only", tagged `locked`, v1) was
  recorded in the cloud-api project's `memory.db`.
- An agent working in the spelunk-oss project ran `spelunk context` /
  `spelunk memory search "SSE"` at session start, per the standard agent
  workflow in `CLAUDE.md`.
- That agent's local `memory.db` (spelunk-oss) had no record of decision #134.
  Nothing in its `spelunk context` output surfaced the Cloud-only constraint.
- An unreviewed cloud-api ADR proposing OSS-server SSE went unchallenged,
  producing work that contradicted #134 and had to be reverted (PRs #379/#384
  closed invalid).

`spelunk link` was already in place between these projects for *code* search
— the registry dependency edge existed — but it had zero effect on memory
visibility.

## Decision

Extend cross-project visibility from the existing registry dependency graph
(`project_deps`) to memory backends, **scoped to `locked` and other
explicitly cross-cutting decisions**, surfaced through `spelunk context` and
`spelunk memory search|list`. Every cross-project result is tagged with its
source project so conflicting decisions between linked projects are
attributable, not silently merged (addendum requirement, see "Source
attribution" below).

### 1. Scope: which entries cross project boundaries

Not all memory should leak across projects — most decisions, requirements,
and handoffs are genuinely local (e.g. "use Next.js" in spelunk-webapp vs.
"never use Next.js" in marketing-site are both correct, for their own
projects). Cross-project surfacing must be **opt-in per entry**, not a blanket
merge of every linked project's full memory.

- A note is **cross-cutting** if it has the tag `locked` (the existing
  convention used for #76/#77/#134-style settled decisions — see
  `spelunk memory list --kind decision` output, e.g. tags
  `v1, cli, remote, progressive-enhancement, locked`) **or** the tag
  `cross-project`.
- Only notes with `kind == "decision"` or `kind == "requirement"` are
  eligible for cross-project surfacing. `handoff` and `question` entries
  remain strictly local — they're inherently session/project-scoped and
  flooding them across projects would be noise.
- `status` must be `"active"` (not `"archived"` / superseded). Superseded
  cross-project decisions must not resurface in a dependent project after
  they're retracted in the source project.

This scoping means: tagging decision #134 `locked` (already done — it's a v1
locked decision) is sufficient for it to become visible from spelunk-oss once
the link exists. No new tagging campaign is required for existing locked
decisions; they already carry the marker this design keys off.

### 2. Registry: reuse `project_deps`, no new table

No schema change to `~/.config/spelunk/registry.db` is required. The existing
`project_deps` edges (populated by `spelunk link`) already express "search
from project A should include project B". Memory visibility reuses the same
edges with the same direction semantics: if A depends on B (`spelunk link B`
run from A), A's `spelunk context`/`memory search` surfaces B's cross-cutting
decisions.

Each `Project` row in the registry has `root_path` and `db_path` (pointing at
the *index* DB, e.g. `.spelunk/index.db`). The memory DB for a project lives
alongside it as `memory.db` in the same directory
(`crates/spelunk-cli/src/cli/cmd/memory/mod.rs` derives
`mem_path = index_db_path.with_file_name("memory.db")` — confirm exact helper
at implementation time, but this sibling-file convention is already used by
`context.rs`'s `args.db` default). So for each dep `Project`, the dep's
`memory.db` path is `dep.db_path.with_file_name("memory.db")`.

**Symmetric link requirement for the failure mode in this incident:** the
2026-06-11 incident is the *root* (or a sibling project) needing visibility
into cloud-api's locked decisions. For the spelunk-oss project to see
cloud-api's decision #134, spelunk-oss must have a `project_deps` edge to
cloud-api (i.e. `spelunk link` run against the cloud-api project from
spelunk-oss, or equivalent from root). This ADR does **not** mandate
auto-linking all five named projects (root, cloud-api, spelunk-oss,
spelunk-webapp, marketing-site) to each other — that's an operational/registry
setup task, not a code change. However, **the implementer should verify (and
if missing, the architect/CoS should direct setup of) `spelunk link` edges
between root ↔ cloud-api, root ↔ spelunk-oss, root ↔ spelunk-webapp, and root
↔ marketing-site at minimum**, so a `locked` decision recorded against `root`
or `cloud-api` is reachable from any sibling that links through root. Direct
sibling-to-sibling links (e.g. spelunk-oss ↔ cloud-api) should be added where
the SSE-incident pattern recurs. File this as a follow-up ops task — see
"Follow-ups" below.

### 3. Query path: `spelunk context` and `spelunk memory search|list`

Both commands gain a **second pass** after querying the local backend:

```
local_results := backend.list(...) / backend.search(...) / backend.search_text(...)
                  // unchanged — existing single-backend call

if not args.local_only:
    deps := registry.get_deps(current_project.id)   // same call search.rs already makes
    for dep in deps:
        dep_mem_path := dep.db_path.with_file_name("memory.db")
        if dep_mem_path doesn't exist: skip (warn on stderr, like search.rs does for missing dep DBs)
        dep_backend := open local sqlite MemoryBackend at dep_mem_path
                        // NOTE: always local sqlite for deps, regardless of
                        // the primary project's configured backend — a
                        // git-notes or remote-server primary backend does not
                        // imply the dep should be queried the same way; the
                        // registry only knows local index/memory DB paths.
        dep_results := dep_backend.list(kind, limit, archived=false, as_of=None)
                        .filter(|n| n.tags.contains("locked") || n.tags.contains("cross-project"))
                        .filter(|n| n.kind in {"decision", "requirement"})
                        .filter(|n| n.status == "active")
        tag each dep_result with source project (see §4)
        append to results, deduplicated (see below)
```

**Filtering happens client-side** (in the CLI, not pushed into SQL) for v1:
the existing `NoteStore::list_filtered` / `search_text` / `search` /
`search_hybrid` queries are called unchanged with the dep's `kind` /
`limit` args, then the cross-cutting filter (`locked` or `cross-project` tag,
decision/requirement kind, active status) is applied in Rust before merging.
This avoids touching `crates/spelunk-core/src/storage/memory/search.rs`'s SQL
in v1. If this proves too slow on large memory DBs (unlikely — memory DBs are
orders of magnitude smaller than index DBs), a follow-up can push the tag
filter into the FTS query.

**Deduplication:** dedupe by `(source_project_root_path, note.id)` — *not* by
note id alone, since ids are only unique within a single project's `memory.db`.
Two different projects can both have a decision with `id=12`; they must not
collide.

**`--local-only` flag:** `spelunk memory search` and `spelunk memory list`
gain a `--local-only` flag mirroring `spelunk search --local-only`
(`crates/spelunk-cli/src/cli/cmd/search.rs:43`), skipping the dep pass
entirely. `spelunk context` gains the same flag for consistency (it currently
has none of `search`'s linking-related flags).

**`spelunk context` specifics:** the `decision` and `requirement` sections
(see `SECTIONS` in `context.rs`) are the only sections that get a dep pass —
`handoff` and `question` sections are local-only per the scoping in §1. The
dep pass runs once per eligible section using the same deps list (computed
once per invocation, not per section).

### 4. Source attribution (addendum requirement)

Every note returned from a linked project's memory store **must** be tagged
with its source project in the output, in both human-readable and JSON forms,
so that conflicting decisions between linked projects are visible and
attributable rather than appearing as one contradictory merged list.

**Schema change — `Note` struct** (`crates/spelunk-core/src/storage/memory/mod.rs`):

```rust
pub struct Note {
    // ...existing fields unchanged...

    /// Set only for notes returned via the cross-project dependency pass.
    /// `None` for notes from the primary/local backend. Contains the
    /// dependency project's display name (mirrors `project_display_name()`
    /// used by search.rs for `SearchResult::project_name`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_project: Option<String>,

    /// Set alongside `source_project`: the dep project's root path, for
    /// disambiguation when two linked projects share a display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_project_path: Option<String>,
}
```

This mirrors the existing `SearchResult::project_name` /
`SearchResult::project_path` fields populated by `annotate_dep_results()` in
`search.rs` — same naming pattern, same purpose, applied to `Note` instead of
`SearchResult`. `record_to_note()` in `note_record.rs` sets both to `None`
(local notes); the cross-project merge step in `context.rs` /
`memory/search.rs` / `memory/list.rs` sets them via `project_display_name(&dep.root_path)`
(reuse the existing helper from `crate::cli::cmd::helpers`).

**Text output — `print_note_summary()`** (`crates/spelunk-cli/src/cli/cmd/memory/mod.rs`):

Append a source badge when `source_project` is set:

```
#12  [decision]  SSE memory stream → Cloud-only  dist: 0.1234  \x1b[36m[from: cloud-api]\x1b[0m
```

Local notes (the common case) render exactly as today — no badge, no output
change, no risk of breaking existing snapshot/golden tests that assert local
output format.

**JSON output:** `source_project` / `source_project_path` are present
(non-null) only on cross-project notes, absent (via `skip_serializing_if`)
on local notes — existing JSON consumers that don't know about the new field
are unaffected; consumers that need conflict resolution can branch on its
presence.

**`spelunk context` JSON shape:** the `sections` array's per-note objects gain
the same two optional fields. No structural change to the top-level
`{"sections": [...], "conventions": [...]}` envelope.

### 5. Conflict surfacing (the actual addendum ask)

This ADR does **not** propose automatic conflict *resolution* (e.g. picking
"the more recent decision wins" or merging bodies) — that's an agent-judgment
call, and collapsing it in the CLI risks hiding a real disagreement an agent
should escalate to Johan. Instead:

- Cross-project decisions/requirements are appended to the result list
  **after** local results, each tagged with `source_project` per §4.
- `spelunk context` (text mode) prints them under the *same* section header
  (e.g. "── Decisions ──") as local decisions, with the `[from: <project>]`
  badge — so an agent scanning the Decisions section for "SSE" sees both the
  local entry (if any) and cloud-api's #134 side by side, and can recognize
  a same-topic / different-project pair.
- No new "Conflicts" section or heuristic similarity matching in v1. If usage
  shows agents still missing cross-project contradictions despite the badge,
  a follow-up can add a lightweight same-topic heuristic (e.g. tag-overlap
  clustering) — out of scope here.

### 6. Output ordering and limits

- Local results are always listed first, preserving today's ordering
  (`list_filtered` order / search distance order).
- Cross-project results are appended per-dep, in registry `project_deps`
  iteration order (same order `search.rs` uses for its deps loop), each
  internally preserving the dep backend's own ordering.
- The existing `--limit` argument applies to the **local** query only (as
  today). Cross-project results are *additional* — they do not count against
  `--limit` and are not truncated by it, because they're pre-filtered to a
  small `locked`/`cross-project`-tagged subset that is expected to be small
  (handful of entries per linked project, not hundreds). If a project
  accumulates enough `locked` decisions that this becomes noisy, that's a
  signal to prune `locked` tags, not to add a separate limit flag in v1.

### 7. Failure modes

- Dep's `memory.db` doesn't exist (project linked for code search but never
  ran `spelunk memory add`): skip silently (no entries to surface), no
  warning — this is the common/expected case for code-only deps.
- Dep's `memory.db` exists but fails to open (corrupt, permissions): warn on
  stderr (`tracing::warn!`, matching `search.rs`'s `"could not open dep DB
  {}: {e}"` pattern), skip that dep, continue with others.
- Registry unavailable / current project not registered: behave exactly as
  today — local-only, no error (matches `search.rs`'s graceful degradation
  when `resolve_project_and_deps` can't find a registry entry).

## What breaks if ignored

The 2026-06-11 incident recurs: any `locked` cross-cutting decision recorded
in one linked project's memory remains invisible to agents working in
sibling/dependent projects, even when `spelunk link` has already been run for
code search. Agents will continue to propose/approve work that silently
contradicts settled decisions, and the only mitigation is the manual
ADR-approval gate (decision: ADR-approval gate, ` agent-comms/inbox` /
`feedback_adr_approval_gate.md`) — a process control compensating for a tooling
gap. This ADR closes the tooling gap; the process gate remains as
defense-in-depth.

## Alternatives considered

1. **Merge all linked projects' memory.db into one shared DB.** Rejected —
   destroys the "each project's memory is locally owned" model, makes
   `spelunk memory supersede`/`archive` ambiguous about which project's
   record is being mutated, and reintroduces exactly the "project A says X,
   project B says not-X, now it's one contradictory record" problem the
   addendum explicitly wants to avoid.

2. **Surface *all* of a linked project's decisions/requirements, unfiltered.**
   Rejected — most per-project decisions are not relevant cross-project
   (e.g. spelunk-webapp's component-library choice has no bearing on
   marketing-site). Unfiltered merging would bury the genuinely cross-cutting
   `locked` decisions in noise and make `spelunk context` output unusably
   long for agents with several linked deps.

3. **New `spelunk memory broadcast` / separate "shared decisions" registry
   table.** Rejected for v1 — adds a new storage concept and a new write path
   (`broadcast` vs `add`) when the existing `locked` tag convention already
   marks exactly the right set of entries. Revisit only if tag-based filtering
   proves insufficient in practice.

4. **Push the `locked`/`cross-project` filter into SQL (new indexed column or
   FTS predicate).** Deferred — v1 does client-side filtering after the
   existing `list`/`search` calls (see §3). Revisit if memory DBs grow large
   enough that this becomes a measurable cost; not expected at current scale.

## Cross-cutting tagging requirement (addendum, restated for memory record)

Any decision or requirement intended to bind agents working in **other**
linked projects must be tagged `locked` (existing convention for settled v1
decisions) or `cross-project` (new tag, for cross-cutting items that aren't
otherwise "locked v1" but still need to propagate — e.g. a security policy
that applies repo-wide). Decisions without one of these tags remain
project-local and are never surfaced outside their origin project, regardless
of `spelunk link` edges. Output for any cross-project result carries
`source_project` / `source_project_path` (or the `[from: <project>]` text
badge) so the consuming agent always knows which project a surfaced
decision/requirement originated from.

## Implementation checklist (for implementer)

- [ ] `Note` struct: add `source_project: Option<String>`,
      `source_project_path: Option<String>` (`crates/spelunk-core/src/storage/memory/mod.rs`,
      `note_record.rs::record_to_note`).
- [ ] `print_note_summary()`: render `[from: <project>]` badge when
      `source_project` is set (`crates/spelunk-cli/src/cli/cmd/memory/mod.rs`).
- [ ] `spelunk memory search` / `spelunk memory list`: add `--local-only`
      flag; after local query, resolve registry deps (reuse
      `resolve_project_context` / `Registry::get_deps`), open each dep's
      `memory.db` as a local sqlite `MemoryBackend`, filter to
      `(kind in {decision, requirement}) && status == "active" && (tags
      contains "locked" || tags contains "cross-project")`, tag with
      `source_project`/`source_project_path` via `project_display_name`,
      append + dedupe by `(root_path, id)`.
- [ ] `spelunk context`: add `--local-only` flag; apply the same dep pass to
      the `decision` and `requirement` sections only.
- [ ] Verify/establish `spelunk link` edges between root and each of
      cloud-api, spelunk-oss, spelunk-webapp, marketing-site (ops task, not
      code — file separately, see Follow-ups).
- [ ] Tests: dep with no `memory.db` (skip), dep with unreadable `memory.db`
      (warn + skip), dep decision tagged `locked` surfaces with correct
      `source_project`, dep decision *without* `locked`/`cross-project` does
      NOT surface, dedupe across two deps that both link to a shared
      grandparent, `--local-only` suppresses all of the above.

## Follow-ups

- Ops task: confirm/establish `spelunk link` edges across
  root/cloud-api/spelunk-oss/spelunk-webapp/marketing-site (see §2). Not a
  code change — can run in parallel with implementation.
- Possible future: lightweight same-topic conflict clustering (§5) if the
  `[from: <project>]` badge alone proves insufficient for agents to spot
  contradictions.
- Possible future: push tag-filter into SQL if memory DB sizes make
  client-side filtering measurably slow (§3, alternative 4).
