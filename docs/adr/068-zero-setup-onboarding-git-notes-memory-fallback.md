# ADR-068: Keep zero-setup onboarding; add a git-notes memory fallback before `init`

**Date:** 2026-07-11
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** completes the product-direction decision that
[ADR-067](067-fail-closed-no-local-project.md) explicitly deferred
("Broader UX direction is out of scope … a separate product
decision"). Builds on ADR-067's isolation floor but **narrows** its
"fail-closed for memory" posture: where ADR-067 refused all memory operations
without a local `.spelunk/` project, this ADR routes `memory add` / `memory
list` to the git-notes backend when the working directory is inside a git repo,
and fails only when there is neither a project DB nor a git repo. Keeps
ADR-004's inference-vs-storage split intact.

## Context

A UAT "walk-the-store" session ran every command in the opening two sections of
`getting-started.mdx` ("First commands — no setup needed" and "Search and memory
together") against a real, populated, previously-un-`init`'d checkout of
`getlago/lago`. The doc's promises are explicit and load-bearing:

- Front matter: *"No API keys or servers required to start."*
- *"First commands — no setup needed … Open a terminal inside any git
  repository. Nothing to configure."*
- *"Memory is stored in git notes — no database, no server, no setup. It travels
  with the repo."*

Several of the walked commands did not behave as advertised, and the doc led
with invocations that are not the zero-setup path. The fix is **not** to retreat
from the zero-setup promise — it is to make the promise true by pointing the
docs at the commands that genuinely work with no `init`, and to make `memory
add` / `memory list` honour the "stored in git notes — travels with the repo"
claim before `init` as well.

### What actually runs before `init` (on `main`, after the global-store fix)

A merged fix closed the ADR-067 global-store residual: `graph`, `chunks`,
`check`, and `explore` now route through `require_project_db` and no longer fall
back to the machine-global `~/.config/spelunk/index.db`. Nothing reads that
global store implicitly any more. Current behaviour in an un-`init`'d, populated
repo:

| Doc command | Real behaviour with no `.spelunk/` project |
|---|---|
| `spelunk search "…"` (**auto**, no `--mode`) | Degrades to `search_live` (ast-grep structural scan) when no index is present. This is the genuine zero-setup search surface; the doc never showed it. |
| `spelunk search "…" --mode text` | Explicit FTS-over-index mode. Fails closed via `require_project_db` with *"no spelunk project here. Run 'spelunk init' first"*. Correct for an index-only mode — the doc simply led with the wrong invocation. |
| `spelunk graph <symbol>` | Falls to the ast-grep `symbol($$$)` live call-site scan (exact, unranked) when no index opens; no longer reads any global store (the global-store fix). Zero results print `No scannable source files under this directory` (empty/umbrella dir, no `init` hint) or `No call-site invocations of '<symbol>' found (live structural scan matches '<symbol>(...)' calls only)` plus a `spelunk init` hint (source present, no match). The empty/umbrella-dir branch still has no `init` hint; the source-present branch gained one in the follow-up fix below. |
| `spelunk graph <file-path>` / `chunks` / `check` / `explore` | Index-backed. Refuse with *"no spelunk project here. Run 'spelunk init' first"* (after the global-store fix). |
| `spelunk memory add` / `list` / `search` | **Currently** all fail closed pre-`init`: `memory/mod.rs:377–380` calls `require_project_db(&cfg.db_path, false)` and bails without a `.spelunk/` dir. This ADR changes `add` / `list` (see D3). |

### The architectural fault line

The code already draws the line the product decision needs:

- **Working-tree-only, index-free, global-store-free** commands: `search "…"`
  (auto→ast-grep), `search --mode ast-grep`, `graph --live` / `graph <symbol>`'s
  ast-grep fallback. ADR-067 D1 exempts ast-grep because it "touches no index and
  no global store," and the global-store fix removed the residual global read. These are the
  real zero-setup surface and are safe to run anywhere.
- **Index-backed** commands: `search --mode text` (FTS), `graph <file>`,
  `chunks`, `check`, `explore`. These need `spelunk index` and correctly refuse
  without a project.
- **Memory**: has two local-only backends already — the default SQLite
  `memory.db` (needs a `.spelunk/` project) and the **git-notes** backend
  (`crates/spelunk-core/src/storage/git_notes/`, `GitNotesBackend`,
  `append_to_git_notes`, ref `refs/notes/spelunk`), today reachable only via the
  explicit `--backend git-notes` flag. The git-notes backend stores each entry
  as a JSON line on the HEAD commit's note and supports `add`, `list`, `get`,
  `count`, `archive`; it returns a clear `BackendUnsupported` for the
  embedding-dependent operations (`search`, `search_hybrid`, `search_text`,
  `search_timeline`) and for `supersede` / graph edges.

That second point is the opening this decision uses: a git-notes-backed
`memory add` / `list` needs **no index, no server, and no `init`** — only a git
repo — which is exactly the "stored in git notes — travels with the repo"
promise the doc already makes. It also lets an engineer borrowed for a quick fix
record a result without indexing a whole (possibly huge) project.

## Decision

**Keep zero-setup as the headline promise. Do not lead onboarding with
`spelunk init`. Make `memory add` / `memory list` work before `init` via the
git-notes backend when a git repo is available, and document only the commands
that genuinely work without an index DB.**

### D1 — keep the zero-setup promise (reverses the earlier "lead with `init`")

`getting-started.mdx` keeps "no setup needed / no API keys or servers to start"
as the headline. `spelunk init` is **not** promoted to step 1, and the
zero-setup pitch is **not** retired. The opening section showcases the commands
that actually run with no `init` (D2); index-backed and semantic examples are
clearly marked as "after `spelunk init`," but they do not displace the
zero-setup framing at the top.

### D2 — a small, honestly-scoped index-free surface, documented as such

`getting-started` is ordered in sections. **The first section is pre-`init`**
and showcases **only** the commands below (the index-free surface). It is
followed by the **`spelunk init`** section, and only *after* that do the
index-backed commands appear (indexed `search` / `--mode text`, the full indexed
`graph`, semantic search, and DB-backed `memory`). "Index-free only" applies to
that opening pre-`init` section, not to the whole document. Keep exactly these
working with **no** `init`:

- `spelunk search "<query>"` (auto mode → ast-grep live structural scan) and
  `spelunk search --mode ast-grep`.
- `spelunk graph <symbol>` (ast-grep live call-site fallback) / `graph --live`.
- `spelunk memory add` and `spelunk memory list` (git-notes backend, per D3).

The docs must showcase the invocations that work as written — bare `search "…"`,
not `--mode text`; `graph <symbol>` / `--live`, not `graph <file>` — and frame
the code-search/graph pieces as a *live structural scan*, not the full indexed
graph/search. Working-tree-only is a hard constraint: none of these may read a
machine-global store (the global-store fix already guarantees this for `graph`).

### D3 — git-notes memory fallback for `add` / `list` before `init` (new feature)

When there is no configured project DB (pre-`init`) but the current directory is
inside a git repository, `memory add` and `memory list` record to (and read back
from) git notes on the nearest repo instead of failing. This lives the "memory
goes with the repo" promise and gives an engineer a place to record a result,
and via `list` visibility into what is stored, without indexing the project.

> **Revised 2026-07-13** (founder correction from the #580 review): the first
> draft of this section resolved backend selection as a precedence ladder whose
> pre-`init` rung made git notes the *store of record* and **skipped** the
> existing git-notes write-through ("no double write"). That reshaped store
> priority for no reason. The corrected model below does not touch store
> priority: it leans on the universal `store_in_git_notes` write-through that
> already runs on every `memory add`, and only stops `add` / `list` from failing
> closed before `init`. #580 implemented the superseded framing and is
> re-scoped to this model.

**The carrier already exists.** `memory add` already appends every new entry as
a line of JSON to `refs/notes/spelunk` on HEAD, *in addition to* its primary
SQLite write, whenever `store_in_git_notes` is true, which is the default
(`Config::store_in_git_notes` in `config.rs`). That append (`append_to_git_notes`
in `memory/add.rs`) is best-effort and non-fatal, and it is the mechanism behind
the product's "memory travels with code" messaging. The only thing that stops it
before `init` is that `memory/mod.rs` resolves the store via
`require_project_db(&cfg.db_path, false)` for **every** subcommand and bails
before `add` ever runs.

**Store roles, reconciled with [ADR-004](004-unified-memory-storage.md).** Git
notes (`refs/notes/spelunk`) are the durable carrier that travels with the repo;
the local SQLite `memory.db` is the queryable index over that carrier. It holds
the embeddings semantic `memory search` needs and is hydrated from the notes
(the `init`-time git-notes import is exactly that hydration step). This
does **not** contradict ADR-004. ADR-004 resolved a *local-vs-server*
split-brain: it makes `memory.db` canonical *relative to a shared team server*
and holds that memory stays local unless an explicit team `server_url` relocates
the store of record. Both `memory.db` and `refs/notes/spelunk` are local to the
repo, so neither leaves the machine and that clause is untouched. ADR-004
adjudicates local vs server, not the relationship between two local carriers;
describing git notes as the carrier and `memory.db` as the index over it sits
entirely inside ADR-004's "memory stays local" domain.

**What changes (`add` / `list` before `init`).** Do **not** fail closed, and do
**not** reshape store priority: the ADR-004 backend order for the primary store
is unchanged (explicit `--backend git-notes` selects git notes; an explicit team
`server_url` selects the remote backend; otherwise the local `memory.db`). The
single change is at the pre-dispatch `require_project_db` bail:

- **`memory add`:** with no `.spelunk/` project but CWD inside a git repo
  (`git rev-parse HEAD` succeeds), skip the absent SQLite primary write and let
  the universal write-through carry the entry to git notes through
  `append_to_git_notes`, the very call that already runs post-`init`. There is
  **one write path** pre- and post-`init`, so every note in `refs/notes/spelunk`
  carries an identical record shape (`schema_version`, timestamps, `remote_id`).
  That uniformity is the robustness win over a separate `GitNotesBackend.add`
  path: it keeps the `init`-time import and plain
  `git notes --ref=spelunk …` inspection consistent.
- **`memory list`:** with no `memory.db`, read the entries back from
  `refs/notes/spelunk`.
- **Fail only** when there is neither a project DB nor a git repo, with the same
  message as the first draft: *"no spelunk project here, and not inside a git
  repo. Run 'spelunk init' first, or run inside a git repository."*

**Double-write guard.** The one case where the primary store already *is* git
notes is an explicit `--backend git-notes`. There, and only there, suppress the
write-through so the entry is not written to `refs/notes/spelunk` twice. In every
other case the primary is SQLite (or, pre-`init`, absent), and the write-through
is the sole notes writer.

Entries recorded before `init` carry no embedding vector (git notes hold none),
which is fine because semantic `memory search` stays gated (D4); the vector is
added when the project is later `init`'d and indexed and the carrier hydrates the
index.

Scope of the fallback is deliberately narrow — **only `add` and `list`**. Every
other memory subcommand (`search`, `timeline`, `show`, `graph`, `since`,
`harvest`, `archive`, `supersede`, `push`, `pull`, `sync`, `reconcile`) keeps
its current pre-`init` behaviour (fail-closed, or its existing server/index
requirement). This matches the git-notes backend's own capability surface, which
already returns `BackendUnsupported` for the embedding-dependent methods.

**Known limitations to carry into implementation (not blockers):**

- **The git-notes read-modify-write race** (`GitNotesBackend` doc-comment /
  issue #185): concurrent `add`s to the same HEAD note can lose a write. That is
  acceptable for the solo, pre-`init` quick-fix use case this fallback targets;
  multi-agent workflows should `init` and use SQLite. State this in the docs, do
  not try to fix it here.
- **Empty repo / no HEAD:** if `git rev-parse HEAD` fails (a repo with no
  commits), the fallback cannot attach a note; treat that as "no git repo
  available" and fall to case 5 with the same message.

### D4 — `memory search` stays index/server-gated, with a better message

Do **not** attempt git-notes-backed semantic memory search (it is hard to do
well, and the git-notes backend already returns `BackendUnsupported` for it).
When no embedder/server and no local index are reachable, `memory search`
returns a clear message pointing at the right next step — `spelunk init`,
`spelunk server start`, or `--mode text` — rather than implying a **team**
`server_url` is required (the current message misleads a solo user).

## Per-item disposition

| Item | Disposition under keep-zero-setup + git-notes fallback |
|---|---|
| **`graph` exact-match only, no signal on zero results** | **Survives, rescoped to a zero-result affordance.** When `graph <symbol>` finds nothing, guide the user to `spelunk graph --live` (structural scan) or `spelunk init` (full graph), optionally a did-you-mean. No global-store risk remains (the global-store fix is merged). Drop any "fuzzy graph before init" goal. **Landed:** the source-present branch now prints the reworded message plus an `init` hint; the empty/umbrella-dir branch was left without one on purpose. |
| **`search --mode text` hard-errors, demands `index`** | **Mooted as a code bug; becomes a getting-started doc fix.** The hard error is correct for an explicit index-only mode. The zero-setup example must use bare `search "…"` (auto→ast-grep); `--mode text` is shown as a post-`init` example. No spelunk-oss change. |
| **ast-grep fallback has no substring/fuzzy** | **Optional enhancement, not a blocker.** Ship a clear "no matches (live structural scan); run `spelunk init` for full search" hint now; treat fuzzy/substring as a later nice-to-have. |
| **Memory scoping** (silent global DB; git-notes not the default backend / no sync consumer) | **Direction changes from fail-closed-refuse to git-notes fallback (D3).** ADR-067 already closed the silent-global-DB leak. This ADR now makes git-notes the **pre-`init` memory path** for `add` / `list`, which reverses the "git-notes is not the default backend" premise for the pre-`init` case. Follow-up: the absent sync consumer and the fact that notes don't travel via push/fetch/clone by default become **material** to the "travels with the repo" promise; see Open questions. |
| **`memory search` misleadingly suggests team `server_url`** | **Messaging fix (D4).** Point at `spelunk init` / `spelunk server start` / `--mode text`, not a team server. Consider defaulting to `--mode text` when no embedder is available. |
| **Git notes don't travel via push/fetch/clone by default** | **Elevated by this decision.** Once git-notes is the pre-`init` "memory that travels with the repo," the promise only fully holds if `refs/notes/spelunk` is push/fetch-visible. Decide whether the fallback (or `init`, or a documented one-time git config) should configure the notes refspec (see Open questions). |
| **Manual git-notes inspection docs** | **Elevated.** More users will now have notes written via the fallback; the inspection docs (`git notes --ref=spelunk …`, and `spelunk memory list`) are the transparency surface. Keep them current. |
| **getting-started rewrite** (marketing site) | **Primary doc deliverable.** Keep the zero-setup framing (D1). Fix the broken examples to use commands that actually work with no `init` (D2): bare `search "…"`, `graph <symbol>` / `--live`, and `memory add` / `list` via the git-notes fallback (D3). Move `--mode text`, indexed graph, and semantic examples into a clearly-marked "after `spelunk init`" section without displacing the zero-setup headline. |
| **New work item — git-notes memory fallback** | **Implement D3** in spelunk-cli: before `init` (no `.spelunk/` project) but inside a git repo, stop `memory add` / `list` failing at the `require_project_db` bail. `add` skips the absent SQLite primary and lets the existing `store_in_git_notes` write-through carry the entry to `refs/notes/spelunk`; `list` reads it back from the notes; explicit `--backend git-notes` suppresses the write-through so an entry is not written twice. Fail only when there is neither a DB nor a git repo. File under spelunk-oss. |

## Non-goals

- **Not** retiring the zero-setup promise (this ADR reverses that earlier
  direction).
- **Not** git-notes-backed semantic `memory search` (D4 keeps it gated).
- **Not** extending the git-notes fallback beyond `add` / `list` — other memory
  subcommands keep their current pre-`init` behaviour.
- **Not** re-introducing a per-directory SQLite memory store outside `.spelunk/`
  (that would reopen the ADR-067 commingling leak; the fallback uses git notes,
  which are scoped to the repo, not a stray global DB).
- **Not** removing or migrating the global `~/.config/spelunk/` store (ADR-067
  left it behind an explicit-only path; unchanged).
- **Not** adding a `--global` flag (ADR-067 D2 reserved it; still deferred).
- **Not** changing the inference-vs-storage split (CLAUDE.md / ADR-004): an
  auto-discovered loopback server remains inference-only and never owns memory.

## Consequences

- **The pitch stays "zero setup."** The opening promise — no API keys, no
  servers, memory that travels with the repo — is kept and made true by pointing
  the docs at the commands that work without `init` and by carrying `memory add`
  / `list` to git notes before `init` through the write-through that already
  ships.
- **`memory add` / `list` gain a new pre-`init` path** (the git-notes
  write-through, now allowed to run with no `.spelunk/` project). This is net-new
  behaviour and a partial reversal of ADR-067's
  fail-closed-for-memory posture: fail-closed now means "fall back to git notes
  if a repo exists, else refuse," not "always refuse without `.spelunk/`."
- **git-notes visibility becomes a promise-load-bearing concern.** Notes not
  pushing/fetching/cloning by default, and the absence of a sync consumer, move
  from cleanup to "does the 'travels with the repo' claim actually hold?", a
  question resolved in Open questions / follow-up rather than silently assumed.
- **The work shrinks to messaging + docs + one new feature.** The `--mode text`
  hard error is a doc fix; memory scoping reframes to the git-notes fallback; the
  `graph` zero-result affordance and the `memory search` message are small
  affordance/messaging fixes; ast-grep fuzzy matching is optional; notes
  visibility and the git-notes inspection docs are elevated; plus the
  one new implementation item (D3).
- **Revisit if:** the "travels with the repo" promise cannot be honoured without
  surprising git config changes (see Open questions), in which case the doc claim
  should soften to "stored locally in git notes" rather than "travels with the
  repo."

## Security implications

- No new trust boundary. The git-notes fallback writes to `refs/notes/spelunk`
  in the user's own repo — no network, no global store, no cross-repo
  commingling (notes are scoped to the repo git resolves from CWD). It does not
  reintroduce the ADR-067 per-directory-global-DB leak.
- The existing secret-scan gate in `memory add` (`contains_secret` on title and
  body, run **before** any persistence) applies unchanged on the git-notes path,
  so no credential reaches the note.
- D2's working-tree-only constraint on the index-free surface is preserved by
  the global-store fix (no machine-global read from `graph`).

## Open questions

- **Does "travels with the repo" require configuring the notes refspec?**
  `git notes` under `refs/notes/spelunk` are **not** pushed, fetched, or cloned
  by default. Because D3 makes those notes the durable carrier for
  pre-`init` memory (not a second copy of a SQLite store of record), the
  "travels with the repo" claim now rests entirely on that ref being visible
  across clones. For the promise to hold across machines / teammates, either
  the fallback path, `spelunk init`, or a documented one-time
  `git config --add remote.origin.fetch '+refs/notes/spelunk:refs/notes/spelunk'`
  (plus the matching push refspec) must make the notes ref travel. Recommended
  direction: have `init` offer to configure the refspec, and until then keep the
  doc claim accurate ("stored in git notes; run
  `spelunk memory list` to inspect") rather than over-promising cross-machine
  sync. Track the notes-refspec question separately; do not block D3's local
  `add` / `list` on it.

## Amendment (2026-07-13): canonical content-addressed identity for memory entries

**Date:** 2026-07-13
**Deciders:** founder (Johan); architect

This amendment fixes the identity model that D3's git-notes carrier and the
`init`-time git-notes import both depend on. It is recorded here, on ADR-068,
because both consumers are ADR-068 work. Both **shipped** keyed on the model this
amendment replaces, so it gates nothing: it supersedes code already on `main`, and
A6 specifies the retrofit. It refines the git-notes v1 surface frozen in
[ADR-059](059-git-notes-v1-format-freeze.md) (additive, backward-compatible) and
the local identity columns added under
[migration 020](../../crates/spelunk-core/migrations/020_memory_uuid.sql).

### A0 – Problem

A memory entry has, today, three identity-shaped values that travel, none of
which is a stable cross-boundary identity:

- **Local i64 `id`** (`notes.id`): an autoincrement rowid. It is machine-local,
  it resets to 1 when a project is re-`init`'d (the DB is recreated), and it is
  numbered independently on every machine. It was never meant to leave the
  process, yet `NoteRecord` serializes it straight into `refs/notes/spelunk`
  (`note_record.rs`, `pub id: i64`), so it leaks across the git-notes carrier.
  Observed live during UAT: two different decisions were both stamped
  `"id":1` in one `refs/notes/spelunk` ref because a re-`init` reset the
  counter between the two writes. `superseded_by: Option<i64>` leaks the same
  unstable rowid, so a supersede edge cannot be resolved after the counter is
  renumbered or on another machine.
- **`remote_id`** (migration 020): the server-minted id. It only exists after an
  entry has been synced to a team server, so it covers the server path alone and
  is absent for the local-only and pre-`init` git-notes cases this ADR targets.
- **`uuid`** (migration 020): a random UUIDv7 minted lazily on first sync and
  pushed as the cloud `external_id` idempotency key. Because it is random rather
  than content-derived, it does not make re-recording idempotent across a
  re-`init` (the DB, and the `uuid`, are gone) or across two machines that
  independently record the same decision.

There is also a **fourth, non-serialized identity**: `memory reconcile` and the
`init` git-notes import already dedup on a computed content hash
(`note_dedup_hash`, `crates/spelunk-cli/src/cli/cmd/memory/reconcile.rs`) — a
`blake3` digest over `\x1f`-delimited `kind`, `title`, `body`, normalized
`tags`, normalized `linked_files`, and `created_at`. Recomputed on demand and
never stored or transmitted, it is the closest thing the codebase has to the
identity specified here; the model below **supersedes it** (A2).

The dedup logic in the git-notes carrier (D3) and the import-on-init hydration
needs one identity that is stable across the local store, the git-notes carrier,
and the server, and that is computable with no server, no sync, and even with
git notes disabled. The i64 rowid, `remote_id`, and the random `uuid` each fail
at least one of those requirements.

### A1 – Decision: a content-addressed `entity_id` is the canonical identity

**The canonical identity of a memory entry is `entity_id`, a content hash
computed from the entry's semantic core. It is the same value on the local
store, in `refs/notes/spelunk`, and on the server, and it is derivable by any
reader from the entry itself with no coordination.** Two parties that
independently record the same decision compute the same `entity_id`;
re-recording an unchanged entry is a no-op keyed on it.

There is **one** id, in one role: `entity_id` is both the stable identity of the
entry and the idempotent write/dedup key. A record whose `entity_id` is already
present is not written or imported again, and supersede edges reference
`entity_id`.

One id suffices because the hashed content cannot change under an entry (A3).
The name says what the value identifies rather than how it is derived, which is
the right level for a field on the wire; the derivation is A2's business.

### A2 – Canonical form and hash (git-independent, cross-language)

`entity_id` is **`sha256`** over the **canonical JSON** of the entry's semantic
core, rendered as a 64-character lowercase hex string.

**Canonical field set (frozen for `schema_version` 1):** exactly three fields,
all strings:

- `body`
- `kind`
- `title`

Everything else is **excluded**: `id`, `remote_id`, `uuid`, `schema_version`,
`created_at`, `valid_at`, `invalid_at`, `status`, `superseded_by`, `source_ref`,
`tags`, and `linked_files`. The exclusions are deliberate:

- `id`, timestamps, `schema_version`, `remote_id`, `uuid` are machine-local,
  volatile, or format bookkeeping. Folding any of them in would reintroduce
  exactly the cross-machine, re-`init`-unstable behaviour this amendment
  removes.
- `status`, `superseded_by`, `valid_at`, `invalid_at` are **mutable state** that
  changes over an entry's life (archive, supersede, temporal validity). Keeping
  them out means the id is a **stable locator**: archiving or superseding an
  entry does not change its id, so those mutations find their target by a
  content-addressed key rather than by the unstable rowid.
- `tags` and `linked_files` are **mutable, machine-variable associative
  metadata**. Two people classifying or linking the same decision differently
  must still land on the same identity, otherwise re-tagging would fragment the
  identity and break the very idempotency this fixes. (This is the direct answer
  to the "a content hash changes on any re-tag, so it is only a version id"
  concern: the fix is to keep mutable metadata out of the hash entirely, not to
  admit it and then paper over the churn with a second id.) A6 specifies how the
  two fields reconcile on a dedup match.

**This supersedes the existing `reconcile.rs` dedup hash (A0) outright.** That
hash keys on six fields — `kind`, `title`, `body`, normalized `tags`, normalized
`linked_files`, `created_at` — where `entity_id` keys on three. **Three fields
drop out of identity, and each drop collapses entries that are distinct today:**

- **`created_at`.** The existing hash folds it in "so two distinct notes with
  identical text don't collapse". Under `entity_id` they *do* collapse:
  convergence across machines and across a re-`init` is only possible if identity
  is independent of when a copy happened to be written, and `created_at` is not
  reproducible by a second party recording the same decision. The accepted cost is
  narrow: deliberately recording a byte-identical `kind`/`title`/`body` twice now
  yields one entry rather than two.
- **`tags` and `linked_files`.** Two entries agreeing on `kind`/`title`/`body` but
  carrying different tags are two entries under the existing hash and **one**
  under `entity_id`. In practice this is the larger of the two changes: the
  existing hash normalizes tag *order*, so only a difference in tag *content*
  forks the key today, and differing tags on the same recorded decision are far
  more likely than byte-identical text recorded twice. So this, not `created_at`,
  is the collapse a real store is most likely to see. It is accepted deliberately,
  and it is the whole point of excluding the two fields above: the alternative is
  that re-tagging fragments identity. Neither tagging is lost — on a match the two
  sets merge by union (A6), so the surviving entry carries both.

A6 specifies the retrofit of the code that computes the superseded hash today.

**JSON canonicalization rules** (so the bytes are identical across the Rust
client and the server, and reproducible by any third-party reader):

1. Object with exactly the three canonical keys, **sorted ascending by Unicode
   code point**: `body`, `kind`, `title`.
2. **Compact:** no insignificant whitespace. `,` between members and `:` between
   key and value, with no surrounding spaces.
3. **UTF-8, no BOM.** String values are emitted as raw UTF-8; non-ASCII
   characters are **not** `\u`-escaped. Only the characters JSON requires are
   escaped: `"`, `\`, and the C0 control characters U+0000 through U+001F.
   Forward slash is not escaped.
4. **No Unicode normalization, no case folding, no whitespace trimming** is
   applied inside the hash: the exact stored bytes of each field are hashed. Any
   input tidying (for example trimming trailing whitespace so trivially
   different inputs collide) is an `add`-time concern applied *before* the record
   is stored, not part of the hash.
5. All three fields are strings, so there is no number, float, or boolean
   canonicalization to specify. Keeping the canonical form string-only is
   intentional and removes that whole class of cross-language divergence.

In Rust this is exactly `serde_json::to_vec` of a `BTreeMap<&str, &str>`
containing the three fields (the `BTreeMap` supplies the sorted keys; serde's
default string encoding supplies rules 2 and 3), then `sha256` of those bytes,
hex-encoded lowercase. Reference form:

```
canonical_bytes = serde_json::to_vec(&BTreeMap::from([
    ("body",  body),
    ("kind",  kind),
    ("title", title),
]))
entity_id = hex_lower(sha256(canonical_bytes))
```

Worked example: an entry with `kind = "decision"`, `title = "HTTP layer"`,
`body = "use axum"` has canonical bytes

```
{"body":"use axum","kind":"decision","title":"HTTP layer"}
```

and `entity_id`
`cc308a1ca5d849191e1710cc9def561377a9ef37e4fcb895e5aa3b1896e43603`.

**Explicitly not the git blob sha.** The id is not coupled to git's object hash.
Git's blob sha frames the content with a `blob <len>\0` header, it is computed
over the whole multi-record note blob rather than one entry, and it is SHA-1
that flips to SHA-256 on opt-in repositories. `entity_id` is a plain `sha256`
over one entry's canonical JSON and is identical whether or not the entry ever
touches git.

### A3 – Why one id is enough: content is immutable, supersede is not a version

A content hash is often only a *version* id, because the hashed content can
change under a stable entity. In this codebase it cannot:

- `kind`, `title`, and `body` are **immutable after creation**. There is no
  `memory edit` subcommand; `open_editor_for_body` composes a *new* entry's body
  at create time. Every in-place `UPDATE notes SET` in the workspace touches only
  `status`, `superseded_by`, `invalid_at`, `uuid`, and `remote_id` — mutable
  state, all excluded from the hash per A2.
- **Supersede does not create a version.** `memory supersede` takes `--old-id`
  and `--new-id` and bails if the new entry does not already exist
  (`crates/spelunk-cli/src/cli/cmd/memory/supersede.rs`): it draws an edge
  between two independently created entries, both of which remain stored with
  their own content and their own `entity_id`. A correction is a new entry that
  archives the old one and links back to it, never a rewrite of the old one.

So no entry can ever hold two different contents, and no "which version wins"
situation exists. A genesis-content id and a current-content id would hold the
same value for every entry that can exist under this model, and the second name
would buy nothing.

**Supersede does not dangle.** The edge is expressed as the superseding entry's
`entity_id`, a content-addressed value, so it resolves correctly after a
re-`init` renumbers rowids and across machines. This replaces the
`superseded_by: Option<i64>` leak.

### A4 – Reconciliation: one canonical identity, not three

To avoid the entry carrying competing identities, the roles collapse as follows:

- **`entity_id` is the single canonical global identity** of a memory entry, on
  every surface (local store, `refs/notes/spelunk`, server).
- The **local i64 `id` is demoted to an in-process rowid only.** It stays the
  SQLite primary key for local joins and is convenient in CLI output, but it is
  **no longer serialized as identity**. Distributed surfaces carry `entity_id`;
  a reader that needs a stable handle uses `entity_id`, never the rowid.
- **`remote_id` becomes a server addressing handle mapped from `entity_id`, not
  a competing identity.** It keeps the one job it does today: `remote_id IS
  NULL` is what marks a row as not yet pushed
  (`crates/spelunk-cli/src/cli/cmd/memory/sync.rs`,
  `crates/spelunk-core/src/storage/memory/sync.rs`). That job is unaffected. The
  server may keep its own rowid or UUID for REST paths and internal joins, but
  correlation of "the same entry" across machines is by `entity_id`;
  `remote_id` maps one-to-one to it and is not used to decide identity.
- **The random `uuid` concept retires.** Its only job is being the `external_id`
  idempotency key on the wire — minted in `ensure_uuid` before push, recorded on
  pull in `apply_remote_note` (`crates/spelunk-core/src/storage/memory/sync.rs`).
  It has no other consumer. Once `entity_id` is the idempotency key that job is
  gone, so the random-value concept retires outright rather than changing:
  `entity_id` makes re-recording idempotent across a re-`init` and across
  machines, which a random value cannot. This reverses the "fresh UUIDv7, not
  content-derived" choice noted in migration 020; the founder directed the
  content-addressed model on 2026-07-13. The server's own internal id generation
  is unaffected. Whether the migration 020 `uuid` *column* is repurposed to hold
  `entity_id` or a dedicated column is added is implementing work's call (A5);
  the concept's retirement is this amendment's.

End state: **`entity_id` = canonical identity (everywhere); local i64 =
in-process rowid; `remote_id` = server addressing handle mapped from
`entity_id`.**

### A5 – What changes on each surface (additive, backward-compatible)

All changes are additive under ADR-059's rules (optional fields, absent reads as
`None`, no existing field changes type or nullability), so `schema_version` stays
`1`. The canonical-form definition in A2 is what `schema_version` 1 pins; any
change to the canonical field set is a `schema_version` bump and a new ADR.

- **`NoteRecord`** (`note_record.rs`, the git-notes and local JSON shape): add an
  additive `entity_id: Option<String>` and, for supersede portability, an
  additive string form of the supersede reference carrying the target's
  `entity_id`. The existing `id: i64` and `superseded_by: Option<i64>` remain for
  backward compatibility but are no longer the identity of record. Because
  `entity_id` is a pure function of `{kind, title, body}`, a reader encountering
  a **legacy blob without `entity_id` recomputes it** from the three fields it
  already has. Absence is fully recoverable; storing the field is an
  optimization (O(1) dedup).
- **Local store:** persist `entity_id` alongside each entry so dedup and edge
  resolution are index lookups rather than recomputations. Whether this reuses
  the migration 020 `uuid` column or adds a dedicated column is left to the
  implementing work; the logical requirement is that `entity_id` is stored and
  uniquely indexed.
- **`/v1` wire types and server rows:** carry `entity_id` as the additive
  canonical identity, mapped to the server's `remote_id` handle per A4. Additive
  and optional, consistent with ADR-059 D2's treatment of `remote_id`.

### A6 – Retrofit of the shipped consumers

Both ADR-068 consumers — the D3 git-notes carrier and the `init`-time git-notes
import — **shipped before this amendment was written**, keyed on the identity
model A0 describes. This section is therefore not a gate on upcoming work. It is
the **specification of a retrofit** of code already on `main`.

What is live on `main` today: `NoteRecord` (`note_record.rs`) still declares
`pub id: i64` with no `skip_serializing_if`, so the machine-local rowid is
serialized unconditionally into **every** git-notes blob the shipped carrier
writes, alongside `pub superseded_by: Option<i64>`. The rowid leak that produced
the observed `"id":1` collision (A0) is present and unfixed. The retrofit below is
what closes it.

- **git-notes carrier (D3):** each entry is identified in `refs/notes/spelunk` by
  its `entity_id`. Appending an entry whose `entity_id` is already present on the
  target note is a no-op; two different entries have different `entity_id`s, so
  the observed `"id":1` collision cannot recur. Supersede and archive locate
  their target by `entity_id`, not by the i64 rowid.
- **import-on-init hydration:** when hydrating `memory.db` from
  `refs/notes/spelunk`, dedup by `entity_id` (recomputed from `{kind, title,
  body}` for any legacy line that lacks the stored field). An entry whose
  `entity_id` already exists locally is not re-inserted. Local rowids are
  assigned fresh on import and are never used to correlate.

#### `note_dedup_hash` is replaced outright

`note_dedup_hash` and its server-side twin `ServerNote::hash()`
(`crates/spelunk-cli/src/cli/cmd/memory/reconcile.rs`) compute the superseded
digest. **`entity_id` replaces both. There is no coexistence, no fallback path,
and no second key**: one function, `entity_id(kind, title, body)` per A2, is the
key at every site that keys a memory entry. The mechanical deltas are:

- **Digest:** `blake3` becomes `sha256`.
- **Input framing:** `\x1f`-delimited concatenation of six fields becomes the
  canonical JSON of three fields (A2).
- **Availability:** a value recomputed on demand, never stored and never
  transmitted, becomes a value stored on the row, uniquely indexed, and
  serialized on the wire (A5). Dedup sets are therefore read from the column
  rather than recomputed over every local row.

The call sites, and what each becomes (line numbers are indicative; the named
function or binding is the durable anchor):

- **Reconcile import dedup** — the `existing_hashes` set, ~`:273`, which filters
  `server.db` candidates down to those absent from `memory.db`. Becomes a set of
  `entity_id`s.
- **Reconcile supersede link resolution** — the `hash_to_local` map, ~`:325`.
  This is the site the identity change actually repairs; specified below.
- **`init` git-notes import dedup** — the `existing` set and the `to_import`
  filter, ~`:622` and ~`:627`, on the `init`-time hydration path. Same
  substitution as reconcile's import dedup, and by the same function.
- **Reconcile discovery-nudge count** — `count_reconcilable`, ~`:704`. Keys on
  `entity_id`, with a user-visible consequence: a `server.db` row differing from a
  local entry only in `created_at`, `tags`, or `linked_files` is no longer counted
  as new, so on unchanged data the nudge count can fall, including from nonzero to
  zero (suppressing the nudge entirely).
- **`ServerNote::hash()`** — the same digest computed over a `server.db` row's raw
  CSV fields. Retires with `note_dedup_hash`. The `ServerNote` fields that fed it
  stay on the struct — `tags` and `linked_files` for the union merge below,
  `created_at` for the created_at-ascending import ordering and for
  `add_note_with_created_at` — but they no longer feed identity.

**Supersede link resolution (~`:325`) — what changes.** This site exists precisely
because server rowids aren't portable: it rebuilds a `hash → local rowid` map to
relink supersede chains after import, because the `superseded_by` rowid read from
`server.db` means nothing in `memory.db`. Under `entity_id` the edge references
the successor's `entity_id` (A3), derivable from the successor's own
`kind`/`title`/`body` — content reconcile already holds for every candidate it
read. The map becomes `entity_id → local rowid`, answered from A5's unique index.
Two behaviours change:

- **A successor present in both stores links to the pre-existing local copy.**
  Today, when the local and server copies of one successor were written at
  different `created_at`s, they hash differently: the server copy fails the dedup
  filter, imports as a *second* copy, and the edge is drawn to that duplicate.
  Under `entity_id` the two copies are one entry and the edge lands on it.
- **An unresolvable successor is counted, not passed over in silence.** Today,
  when the successor is not among the rows read from `server.db`, the lookup guard
  simply yields nothing: the entry imports archived with no link and
  `skipped_archived_supersede_unresolved` is **not** incremented — that counter
  covers only the narrower case where the map lookup itself misses. Identity does
  not eliminate this residue. If the successor row is absent from both stores its
  content, and therefore its `entity_id`, is not derivable by any scheme and the
  edge genuinely cannot be drawn. The requirement is that the case is **reported**:
  an unresolvable supersede target is counted in the reconcile summary rather than
  dropped silently. Under `entity_id` a nonzero count means a real data gap rather
  than an identity artifact.

**The parity test's new invariant.** The existing
`dedup_hash_parity_between_reconcile_and_init_import` test pins two independent
digest computations against drift, asserting that `note_dedup_hash` (a local row)
and `ServerNote::hash()` (a `server.db` row) agree for identical content whose CSV
fields arrive in a different order. Under `entity_id` there are no longer two
computations to reconcile: **both entry points call one shared
`entity_id(kind, title, body)`, so they cannot drift by construction.** The
replacement test asserts the stronger property that shared function makes
available — two entries agreeing on `kind`/`title`/`body` and differing in *every*
excluded field (tag *content*, not merely tag order; `linked_files`; `created_at`;
`status`) yield the same `entity_id`, and the reconcile and `init`-import paths
both key on that value.

**Precondition: backfill.** This retrofit lands on stores already holding rows
with no `entity_id`, and A5's unique index will collide with any pre-existing rows
that agree on `kind`/`title`/`body` but are distinct today under the six-field
hash — exactly the entries the three dropped fields (A2) used to keep apart.
Populating the column on existing rows and resolving those collisions is being
decided separately, is a precondition of this retrofit, and is deliberately not
designed here.

**Reconciling a dedup match.** On a match, the two copies agree on
`kind`/`title`/`body` by construction and may differ on everything else. The
existing merge semantics decide the outcome, extended field-wise; they are not
replaced:

- **`status` and supersede links** reconcile as they do today: a tombstone
  archives the local copy and archival is never undone
  (`apply_remote_note`, `crates/spelunk-core/src/storage/memory/sync.rs`).
  Supersede links travel as `entity_id` per A4.
- **`tags` and `linked_files` merge by set union** — the existing Add-Wins
  OR-Set rule applied one level down, to the two fields that are literally sets:
  an incoming tag is added, no local tag is dropped. Union is order-insensitive,
  idempotent, and convergent, so any import order across any number of machines
  reaches the same result. This is what makes A2's exclusion of them from
  identity coherent: both taggings of a decision land on one entry and both
  survive, rather than one silently winning. As with add-wins at entry level,
  removal does not propagate; there is no tag-removal surface today, so nothing
  regresses.
- **`kind`/`title`/`body` are never written on a match** — they are equal by
  construction, and entry content is append-only.

### A7 – Non-goals, consequences, security

- **Non-goal:** building the git-notes-as-sync consumer (still out of scope per
  ADR-059) or changing the server's internal id generation. This amendment
  defines identity; it does not add a reconciler.
- **Non-goal:** admitting `tags`, `status`, or `linked_files` into identity. They
  stay mutable metadata on the record, merged per A6.
- **Non-goal: in-place `title`/`body` edit.** If it is ever added, the dedup key
  becomes the pair (`entity_id`, current-content hash) — a second entity later
  amended *to* the same content would otherwise collide with the first entity's
  `entity_id` — and that call belongs to the ADR that adds edit, with the real
  feature in front of it. **Version counters** (not derivable from the entry:
  two machines amending offline both mint "v2", reintroducing the collision this
  amendment removes) and **hash/block chains** are rejected outright.
- **Consequence:** identity is now derivable and stable. Re-`init`, offline use,
  and independent recording of the same decision on two machines all converge on
  one id with no server and no coordination. The i64 rowid can be renumbered
  freely without affecting identity or edges.
- **Security:** `entity_id` carries no authority; like `remote_id` it is an
  opaque identity string, and read/write authorization on a shared server is
  unchanged and remains governed by [ADR-056](056-oss-server-tenancy-model.md)
  (single trust domain, shared key). Hashing `title` and `body` exposes nothing
  new: the id always travels next to the very content it is derived from (the
  full body is in the same note line or row), so it reveals nothing a reader of
  the entry does not already hold. `sha256` collision resistance makes an
  accidental id clash between two genuinely different entries negligible. The
  existing pre-persistence secret scan is unaffected; identity is computed from
  the same fields that scan already gates.

## Amendment (2026-07-18): entity_id backfill and uniqueness promotion for existing local stores

**Date:** 2026-07-18
**Deciders:** founder (Johan); architect

A6's retrofit shipped with `entity_id` populated only on new rows: migration
`023_memory_entity_id.sql` adds the column plus a **non-unique** index by
explicit design, and its own comment names the reason: "the backfill rule is a
separate open decision" and "a UNIQUE index would abort the migration." A6
flagged the same gap directly: "Populating the column on existing rows and
resolving those collisions is being decided separately, is a precondition of
this retrofit, and is deliberately not designed here." This amendment is that
decision.

`reconcile.rs` already implements the collapse mechanics this backfill needs
(`collapse_candidates`, `MergedNote::absorb`, `MemoryStore::union_tags_and_files`,
`MemoryStore::set_superseded_by`), but only for *incoming* `server.db`
candidates, before they are ever written to `memory.db`. Rows already resident
in `memory.db` are, by that module's own design, left untouched: duplicates are
folded into `entity_to_local` via `.entry().or_insert()` over rows ordered
`created_at ASC`, so the oldest local row silently becomes the edge target and
nothing merges or is deleted. This amendment applies the same merge rule
intra-table, to rows already sitting in `memory.db`.

### B1 – Collapse existing duplicates, not keep-both under a relaxed constraint

Rows sharing an `entity_id` are merged, not left in place indefinitely under a
non-unique index. Keeping both would key local dedup on something other than
`entity_id`, reopening the cross-machine convergence problem A2 closes and
leaving `memory.db` permanently out of step with the "entity_id is the key"
contract that reconcile, init-import, and server sync already code against.

For each group of `notes` rows sharing an `entity_id`:

- **Survivor**: the row with the earliest `created_at` in the group, matching
  the first-seen convention `all_notes_for_dedup` already uses (rows ordered
  `created_at ASC`, folded via `entity_to_local.entry().or_insert()`).
- **`tags` / `linked_files`**: union, add-wins, order of first appearance, the
  same rule `union_tags_and_files` and A6 use for the incoming-candidate case.
- **`status`**: archived sticks. If any row in the group is archived, the
  survivor becomes archived; if none are, the survivor's own status is
  unchanged.
- **Survivor's own `superseded_by`**: if it is `NULL` and another row in the
  group carries a non-null value, the survivor adopts it. If two rows in the
  group carry conflicting non-null values, the earliest-created one wins
  deterministically and the run logs a warning; it does not error (rows share
  content by construction, so this is expected to be rare).
- **`superseded_by` edges elsewhere in `notes` pointing at a loser**: every row
  whose `superseded_by` targets a loser is rewritten to the survivor's id
  before the loser is deleted, mirroring `set_superseded_by`.
- **Self-edge guard**: a rewrite that would set a row's `superseded_by` to its
  own id is dropped to `NULL` instead, mirroring reconcile's existing
  `if succ_local_id == *local_id { continue; }` guard.
- **Losers are deleted**, along with their `note_embeddings` row if present
  (the `vec0` virtual table carries no foreign key, so this is an explicit
  delete). No embedding merge: two vectors have no meaningful union, and
  embeddings sit outside A6's merge scope.
- The survivor's own `id` (rowid), `remote_id`, and sync bookkeeping are
  untouched; per A4 the rowid is process-local, not identity, so which specific
  row happens to survive does not matter downstream.

### B2 – `spelunk memory dedupe`: explicit, not folded into a silent migration

The collapse in B1 is the first operation in this codebase that deletes rows
already resident in `memory.db`, so it ships as its own command rather than
running invisibly inside routine `Database::open`. This is stricter than
`memory reconcile`'s own posture, not merely consistent with it: reconcile's
collapse only ever touches candidate rows before their first write to
`memory.db`; it never deletes a row already stored there.

A new `spelunk memory dedupe` subcommand, sibling to `reconcile`, with the same
flag and summary shape as `MemoryReconcileArgs`:

- `--dry-run` (bool, default false): detect and report duplicate groups, write
  nothing.
- `--format text|json`, the same convention `reconcile` uses.
- Summary fields: `total_notes`, `duplicate_groups`, `rows_collapsed`,
  `tags_merged`, `linked_files_merged`, `supersede_edges_repointed`,
  `supersede_self_edges_dropped`.
- One `BEGIN IMMEDIATE` / `COMMIT` transaction per run, mirroring
  `import_batch`. Any error mid-run rolls back: `memory.db` is left exactly as
  it was, and the command reports the error rather than a partial summary.
- Never invoked automatically. `spelunk init`, `spelunk memory add`, and every
  other automatic path continue to leave existing duplicate rows alone, exactly
  as migration 023's own comment already documents ("those duplicates are
  harmless and are left in place").

### B3 – Migration shape: two independently-safe steps, no hard-abort

Split into a step that is always safe to run automatically and a step that is
conditional on the first having fully resolved duplicates. Reuses the
`apply_dim_upgrade_migration` idiom already in `db.rs` (the 768-to-896
embedding-dimension upgrade): a Rust-side, marker-guarded, conditional step
that runs at `Database::open` and never issues a blind SQL `ALTER` /
`CREATE UNIQUE INDEX` that can hard-fail the whole open.

**Step A, populate `entity_id` (unconditional, no decision risk).** At
`Database::open`, after existing migrations: select rows where `entity_id IS
NULL`, compute `entity_id()` in Rust (`sha256` is unavailable to raw SQL, so
this cannot be a plain `.sql` migration file), `UPDATE notes SET entity_id =
?1 WHERE id = ?2`. Idempotent: an interrupted run simply leaves the remaining
rows `NULL` for the next open to pick up. Cannot fail on a constraint, because
migration 023's index stays non-unique for this step.

**Step B, promote the index to UNIQUE (conditional).** After Step A, at
`Database::open`: scan for any `entity_id` shared by more than one row.

- **Zero duplicate groups**: `DROP INDEX idx_notes_entity_id; CREATE UNIQUE
  INDEX idx_notes_entity_id ON notes(entity_id) WHERE entity_id IS NOT NULL;`,
  then record a marker (mirroring the `schema_int8_embeddings` marker table) so
  later opens skip the scan.
- **One or more duplicate groups**: no-op. The existing non-unique index stays
  in place (no regression versus today), and one actionable line is logged
  naming `spelunk memory dedupe` as the next step. This is what satisfies "must
  not hard-abort and brick an existing `memory.db`": the store stays fully
  functional indefinitely until the user opts into `dedupe`.
- Both checks re-run on every open until promotion succeeds; the row-scan cost
  is bounded, and Step A already makes the steady-state case a fast no-op
  query.

### B4 – Non-goals, consequences, security

- **Non-goal:** an automatic backfill-and-delete inside routine `Database::open`
  with no explicit user action. B2 makes the deletion deliberate and
  user-invoked.
- **Non-goal:** merging or deduplicating embeddings. Losing a loser's embedding
  on collapse is accepted; the survivor's own embedding, if it has one, is
  untouched.
- **Consequence:** a store with duplicate `entity_id` groups keeps its
  non-unique index and keeps working exactly as it does today; every command
  that does not call `dedupe` is unaffected by this amendment. Uniqueness is
  opt-in, not forced on an existing store.
- **Consequence:** once `dedupe` collapses a store to zero duplicate groups and
  the store is reopened, `idx_notes_entity_id` promotes to UNIQUE and stays
  enforced from then on; a later insert that would collide is a constraint
  violation, not a silent duplicate.
- **Security:** `dedupe` deletes rows the user already owns locally; there is
  no new trust boundary and no data leaves the machine. The transaction posture
  (a single `BEGIN IMMEDIATE` / `COMMIT`, no partial summary on error) is the
  control against a partially-applied merge corrupting `memory.db`; no further
  mitigation is needed because the operation is local, explicit, and confined
  to a single all-or-nothing write.

## Amendment (2026-07-18): `add_note` collision handling once `entity_id` is UNIQUE

**Date:** 2026-07-18
**Deciders:** founder (Johan); architect

The previous amendment promotes `idx_notes_entity_id` to UNIQUE once a store
reaches zero duplicate `entity_id` groups, and accepts as a consequence that
"a later insert that would collide is a constraint violation, not a silent
duplicate." It fully specified the collapse of existing duplicates
(`spelunk memory dedupe`), but left one path unspecified: an ordinary
`MemoryStore::add_note` or `add_note_with_created_at` call for content whose
`kind`/`title`/`body` already matches a stored row, submitted after the index
has promoted. Review of the implementation found that this path hits the bare
SQL error directly: `spelunk memory add` for byte-identical content returns
`Error: UNIQUE constraint failed: notes.entity_id` and exits 1, contradicting
this ADR's own framing that recording identical content twice "yields one
entry."

### C1 - Reuse the existing row instead of erroring

`add_note` and `add_note_with_created_at` catch a UNIQUE-constraint failure on
`notes.entity_id` specifically (matched by the SQLite error message; no other
column either function populates carries a colliding UNIQUE index, since
`uuid` and `remote_id` are both left `NULL` on these insert paths and their own
partial UNIQUE indexes exclude `NULL`), and recover rather than propagate:

- Look up the existing row's id by `entity_id`.
- Merge the call's `tags` and `linked_files` into that row via the existing
  `union_tags_and_files` (add-wins, order of first appearance), the same rule
  this ADR already uses for reconcile's incoming-candidate collisions and for
  B1's intra-table collapse.
- Return the existing row's id instead of inserting a new row.
- Do not touch the existing row's `status` or `superseded_by`. A plain `add`
  call carries neither as an update target the way B1's intra-table collapse
  does for two independently-created rows already resident in the store, so
  there is nothing to reconcile there beyond tags and linked_files.

This is an insert-then-recover design, not a lookup-then-skip one: the
function still attempts the INSERT first and falls back to the merge path
only if SQLite actually rejects it. A proactive lookup before every insert
would also silently change behavior before the index is promoted, when a
store can legitimately hold several rows sharing one `entity_id` (the exact
condition `dedupe` exists to resolve). The test suite for B1-B3 builds such
rows directly via `add_note`/`add_note_with_created_at` against a
not-yet-promoted index and must keep doing so unchanged; an insert-then-recover
design only ever activates once SQLite's own UNIQUE index actually rejects a
write, so it cannot fire before promotion regardless of how many rows already
share an `entity_id`.

`add_note_superseding` (the `--supersedes` path) is out of scope here: its
INSERT statement does not populate `entity_id` at all, so it cannot violate
this index today. That is a related gap in the identity model, not a
consequence of this decision, and is tracked as a separate follow-up.

### C2 - CLI output distinguishes the two outcomes

`MemoryBackend::add`, and the underlying `add_note`/`add_note_with_created_at`,
report whether the call inserted a new row or reused an existing one, so
`spelunk memory add` can tell the user which happened:

- New row: unchanged, `Stored [{kind}] #{id}: {title}`.
- Reused row: `Already recorded as [{kind}] #{id}: {title}`, using the
  existing row's id.

The git-notes write-through carrier is unaffected: it appends unconditionally
regardless of whether the SQLite store deduped, matching its existing role as
an append-only audit trail that `reconcile` already knows how to collapse on
import.

### C3 - Consequences

- A `memory add` call is idempotent on content identity once a store's index
  has promoted: repeating the same `kind`/`title`/`body` never errors,
  extending this ADR's "yields one entry" framing to every insert path, not
  only to `dedupe`'s collapse of rows already resident in the store.
- Before promotion, behavior is unchanged: a store may still accumulate
  duplicate rows exactly as it does today, resolved later by an explicit
  `spelunk memory dedupe` run.
- `add_note_superseding` colliding with the promoted index remains possible in
  principle once its own identity gap (not setting `entity_id` on insert) is
  closed; that gap is not closed by this amendment.

## Amendment (2026-07-19): `add_note_superseding` identity gap and Step A hardening

**Date:** 2026-07-19
**Deciders:** founder (Johan); architect

Two gaps in the supersede path, found in the same review pass because both
sit on the identity-model surface these amendments already cover.

**Provenance check (2026-07-19).** At the time of this amendment, the third
amendment's Step A/B (`entity_id_migration.rs`) and `spelunk memory dedupe`
exist only on an in-progress branch, not yet on `main`. The fourth
amendment's C1 (`add_note`/`add_note_with_created_at` insert-then-recover) is
speced but has no Rust implementation anywhere yet; it is landing on that
same branch. This amendment's E1-E3 are therefore corrections to land **in
the same implementation pass** as that work, not patches against
already-shipped code.

### E1 — `add_note_superseding` gains `entity_id` and the same insert-then-recover as `add_note`

The fourth amendment's C1 explicitly scoped `add_note_superseding` out:
"its INSERT statement does not populate `entity_id` at all, so it cannot
violate this index today... tracked as a separate follow-up." This is that
follow-up.

`add_note_superseding` (`crates/spelunk-core/src/storage/memory/edges.rs`)
computes `entity_id` at insert time exactly like `add_note` /
`add_note_with_created_at`:

```rust
crate::storage::entity_id::entity_id(kind, title, body)
```

added to its `INSERT INTO notes` column list. This closes the root cause: no
future `--supersedes`-created row is ever `entity_id = NULL`.

Once this INSERT populates `entity_id`, it is subject to the same UNIQUE
constraint C1 gave `add_note`, so it needs the same insert-then-recover
handling, not a bare `INSERT` that can now fail: attempt the insert; on a
UNIQUE-constraint failure on `notes.entity_id` specifically, look up the
existing row by `entity_id` and merge `tags`/`linked_files` into it via the
existing `union_tags_and_files`, exactly as C1 specifies, **and use that
existing row's id as the successor** for the archive-`OLD` step that follows
(the transaction's second statement — `UPDATE notes SET status='archived',
superseded_by=?2, ... WHERE id=?1 AND status='active'` — must run against
whichever id is authoritative: the freshly inserted row, or the reused
existing one). Return type changes to expose both the id and whether a fresh
row was created, mirroring the fourth amendment's C2 (so the CLI can
distinguish "created" from "reused" here too). `add_note_superseding`'s own
archive-`OLD` UPDATE must also report whether it actually changed a row — a
signal the separate follow-up work on re-superseding an already-archived
entry will also need.

### E2 — Step A backfill hardens against a collision it can now hit

The third amendment's B3 states Step A "cannot fail on a constraint, because
migration 023's index stays non-unique for this step." That is true only for
a store's *first* pass through Step A, before Step B has ever promoted the
index. It is not true in general: Step A and Step B both run, unconditionally,
on **every** `MemoryStore::open` — not just the first. Once a store has
already been promoted to UNIQUE by an earlier open, any row that reaches Step
A still `entity_id IS NULL` (a row inserted by *some* path that predates E1's
fix, or by any future path that has the same gap) hits Step A's bare
`UPDATE notes SET entity_id = ?1 WHERE id = ?2` on a now-UNIQUE index. If the
computed value collides with an existing row's `entity_id`, that `UPDATE`
raises a UNIQUE-constraint error with no handler, which propagates out of
`backfill_entity_ids` via `?` and hard-fails `MemoryStore::open` itself —
bricking every `spelunk` command against that store.

Step A's per-row `UPDATE` catches a UNIQUE-constraint violation on
`notes.entity_id` specifically and, on that error only, skips the row
(leaving it `NULL` for a future `dedupe`-then-retry) and logs one actionable
warning naming the affected row id and pointing at `spelunk memory dedupe` —
reusing Step B's existing message shape. Any other error from the `UPDATE`
still propagates unchanged. Step A must never hard-abort `open`, matching the
third amendment's own stated invariant for Step B; this closes the one case
where Step A did not yet live up to it.

### E3 — A second latent NULL-`entity_id` insert path found: `apply_remote_note` — flagged, not fixed here

Grepping every `INSERT INTO notes` in `spelunk-core`/`spelunk-cli` found a
second path that never populates `entity_id`:
`MemoryStore::apply_remote_note` (`crates/spelunk-core/src/storage/memory/sync.rs`),
the cloud-pull idempotency path for an explicit team `server_url`. Unlike
`add_note_superseding`, this one is not a same-shape fix: its own doc comment
states an **Add-Wins/keep-both** posture ("pulled entries are added, never
overwriting local ones"), which predates `entity_id` and may not compose
cleanly with C1's merge-on-collision behavior — a pulled row and a locally
authored row can legitimately share content but arrive by different paths,
and which posture is correct there is its own question, not a mechanical
copy of E1. **Not decided or fixed by this amendment.** Filed as its own
follow-up task (spelunk-oss, to be created by the EM/architect next), scoped
to: (a) whether `apply_remote_note` should set `entity_id` at insert
time, and (b) whether a collision there should merge (C1-style), keep-both
(status quo, requiring the index to stay non-unique for this path, which
conflicts with E2's premise), or something else. Until that is decided, E2's
Step A hardening is what keeps this path from being able to hard-fail `open`
in the meantime — this is additional justification for shipping E2 regardless
of E1.

### E4 — Non-goals, consequences, security

- **Non-goal:** deciding `apply_remote_note`'s `entity_id`/collision posture
  (E3) — a separate task, not this amendment.
- **Consequence:** `MemoryStore::open` cannot be hard-failed by Step A
  regardless of which insert path left a row `entity_id = NULL`, closing that
  hard-fail risk, though E3's path remains an open question for its own
  collision semantics.
- **Security:** no new trust boundary. E1/E2 only change how already-local
  SQLite operations recover from a constraint violation; no new data leaves
  the machine.

A separate, related gap in the supersede path — re-superseding an
already-archived entry, and how a conflicting `superseded_by_entity_id`
should fold at read time — is being decided and implemented independently,
with its own ADR-068 amendment once that work lands.
