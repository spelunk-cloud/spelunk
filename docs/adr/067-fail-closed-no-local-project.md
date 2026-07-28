# ADR-067: fail-closed when no local project (no silent global memory store)

**Date:** 2026-07-10
**Deciders:** founder (Johan), architect

## Context

Running a memory or search command in a directory that was never
`spelunk init`'d silently reads and writes a single shared global datastore
under `~/.config/spelunk/` (`index.db` + `memory.db`), with no per-repo
isolation and no warning. Every un-scoped repo on the machine commingles its
decisions into the same file.

### The silent fallback

`Config::db_path` defaults to `~/.config/spelunk/index.db`
(`crates/spelunk-core/src/config.rs:366`). Path resolution then falls back to
that default whenever no local project is found:

- `resolve_db(explicit, cfg_default)`
  (`crates/spelunk-core/src/config.rs:224`) returns `cfg_default` (the global
  `index.db`) when `--db` is absent **and** `find_project_db(&cwd)`
  (`config.rs:176`, walks up for `.spelunk/index.db`) finds nothing.
- `resolve_project_context` (`crates/spelunk-core/src/registry.rs:369`) has the
  same terminal fallback to `resolve_db(None, cfg_default)` (`registry.rs:402`).

Memory commands derive their store from that path:
`mem_path = resolve_db(None, &cfg.db_path).with_file_name("memory.db")`
(`crates/spelunk-cli/src/cli/cmd/memory/mod.rs:375`). With no local `.spelunk/`,
`mem_path` resolves to `~/.config/spelunk/memory.db`. `open_memory_backend`
(`crates/spelunk-core/src/storage/mod.rs:85`) then returns a plain
`LocalMemoryBackend` over that global file, with no isolation and no notice. The
same silent fallback is reached by `memory add/list/search/…` (via the dispatch
above), `context` (`crates/spelunk-cli/src/cli/cmd/context.rs:88`), the `Sync`
arm (`crates/spelunk-cli/src/main.rs:84`), `check`
(`crates/spelunk-cli/src/cli/cmd/check.rs:94,161`), `status`
(`crates/spelunk-cli/src/cli/cmd/status.rs:73,256`), and index-backed `search`
(`resolve_project_and_deps` in `crates/spelunk-cli/src/cli/cmd/search.rs`).

`spelunk index <path>` is not affected: it always writes
`<root>/.spelunk/index.db` (`crates/spelunk-cli/src/cli/cmd/index/mod.rs:88`),
creating a local project rather than falling back to the global path. It is the
project-creation command and stays exempt.

### Backend mislabel in `spelunk status`

`spelunk status` prints two memory lines that can disagree. The text path prints
the real backend once (`b.backend_kind()`, `status.rs:257`), but
`print_tier_section` (`status.rs:334`) also prints a memory label derived from
the capability tier, not the resolved backend: the Offline branch hardcodes
`memory  git-notes (local)` (`status.rs:344`) and the Server branch hardcodes
`git-notes + server sync` / `git-notes (local)` (`status.rs:371`). The default
resolved backend is `sqlite` (`open_memory_backend` returns `git-notes` only
under `--backend git-notes`, and `remote` only for an explicit cloud-routed
`server_url`), so the tier-derived label asserts `git-notes` even when the live
store is the global SQLite `memory.db`. This is a correctness bug independent of
the isolation leak.

## Decision

Silently sharing an un-scoped global cross-project store from a directory with
no local project is wrong regardless of product direction. Fail closed.

### D1 – refuse when there is no local project

`memory add/list/search`, index-backed `search` (semantic/hybrid and any
index-write), and `context` MUST refuse to run in a directory that has no local
`.spelunk/` (walking up from CWD, worktree-aware), and MUST NOT fall back to the
global `~/.config/spelunk/` store. The error is clear and actionable:

```
no spelunk project here — run 'spelunk init' first
```

The presence signal is the `.spelunk/` **directory**, not `index.db`
specifically: `spelunk init --no-index` creates `.spelunk/config.toml` with no
index yet, and memory does not need an index. So the guard walks up for a
`.spelunk/` directory rather than reusing `find_project_db` (which requires
`index.db`). The walk is worktree-aware (main-worktree root, matching
`resolve_main_worktree_root`) so linked worktrees resolve to the same
`.spelunk/`.

An explicit `--db <path>` remains a deliberate override and is exempt from the
guard (it names a store outright, so no silent fallback is involved).

### D2 – any global/cross-project view is explicit opt-in

The global store is not removed. Reaching it from an un-init'd repo is no longer
the silent default; it becomes an explicit choice. If a cross-project or global
view is later wanted it MUST be requested explicitly (e.g. a `--global` flag)
that, when set, restores today's `~/.config/spelunk/memory.db` path. Absent that
flag, no command touches the global store from an un-init'd repo. This ADR does
not add the flag; it fixes the default to fail closed and reserves the explicit
path for a future change.

### D3 – `spelunk status` reports the actually-active backend

The memory backend label in `spelunk status` MUST reflect the resolved backend
(`backend_kind()` -> `sqlite` / `git-notes` / `remote`), never a value inferred
from the capability tier. The hardcoded `git-notes …` strings in
`print_tier_section` (`status.rs:344,371`) are removed; the single truthful
memory line is sourced from the opened backend. `status` in a directory with no
local project reports that there is no project (consistent with D1) rather than
describing the global store as if it were the current project's.

## Scope and non-goals

- **Broader UX direction is out of scope.** Whether spelunk should engineer true
  zero-setup usage or lead with `init` is a separate product decision.
  D1/D2/D3 are the minimal fail-closed correctness fix and
  foreclose neither direction: a future zero-setup design can define its own
  scoped store, and a lead-with-init design already matches this behaviour.
- **The global store is not deleted or migrated.** Existing data under
  `~/.config/spelunk/` is left untouched. Only the silent default path to it is
  closed.
- **No change to backend selection semantics** (`open_memory_backend`): sqlite
  local, git-notes under `--backend git-notes`, remote under an explicit
  cloud-routed `server_url`. D3 only fixes how the resolved choice is reported.

## Consequences

- **Fixed:** an un-init'd repo can no longer read or write another repo's
  decisions through the shared global file; `spelunk status` no longer asserts a
  backend the live store is not using.
- **Behaviour change:** commands that previously "worked" in an un-init'd repo
  by silently using the global store now refuse with an actionable message. This
  is intended: the prior behaviour was a data-isolation leak, not a feature.
- **Revisit if:** an explicit global/cross-project view is specced, which adds
  the opt-in flag described in D2 and its own tests.

## Security implications

- Closes a cross-project data-commingling path: decisions authored in one repo
  no longer land in a machine-global store that every other un-scoped repo reads
  and writes.
- Fail-closed is the safe default under any future direction: the guard denies
  by default and only the explicit `--db` override (or a future explicit
  `--global`) reaches a store outside the current project.
- No new trust boundary is introduced; shared-server authorization is unchanged
  ([ADR-056](056-oss-server-tenancy-model.md)).

## Status

P0 data-isolation fix. Implementation is gated on founder approval of this ADR
and tracked separately.
