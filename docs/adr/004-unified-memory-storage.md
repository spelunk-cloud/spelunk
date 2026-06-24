# ADR-004: One Way to Store Memory — Resolving the Local Storage Split-Brain

**Status:** Approved
**Date:** 2026-06-11
**Context:** Escalated by Johan on PR #386 (ADR-003) line 88: *"We've said that memory lives in the server, but it seems we've not made that the case when we have the auto-discovery server running locally. There should be one way of storing memory."* This is out of scope for ADR-003 (cross-project visibility) and is captured here as its own decision. Builds on the three-tier product architecture (decision #75), the CLI backend-selection order (decision #76), and the progressive-enhancement principle (decision #77).

## Problem

There is no single way memory is stored when a **local** `spelunk-server` is running. Today the same `spelunk memory` invocation can read from one store and write to another, because the read and write paths pick their backend differently.

### Two storage locations

1. **Local SQLite `memory.db`** — `resolve_db(None, &cfg.db_path).with_file_name("memory.db")`, i.e. `.spelunk/memory.db` next to the project's index DB. Owned by `MemoryStore` / `LocalMemoryBackend` in `crates/spelunk-core/src/storage/memory/`.
2. **Server `server.db`** — the auto-spawned daemon's database at `~/.local/state/spelunk/server.db` (`ensure_server_running()` in `crates/spelunk-cli/src/cli/cmd/server.rs`, passed via `build_daemon_args` → `--db`). Owned by `ServerDb` in `crates/spelunk-server/src/db.rs`, written/read over HTTP by `RemoteMemoryBackend`.

These are physically distinct SQLite files with independent `memory_entries` tables. Nothing syncs them.

### Two backend-selection paths

`open_memory_backend(cfg, mem_path, override)` (`crates/spelunk-core/src/storage/mod.rs`) selects:

1. `git-notes` if `backend_override == Some("git-notes")`;
2. `RemoteMemoryBackend` if `cfg.server_url` is set (a **team/remote** server);
3. otherwise `LocalMemoryBackend` over `memory.db`.

Crucially, this function only knows about `cfg.server_url` — the explicit **team** server. It has no knowledge of the **auto-discovered local** server. That knowledge lives one layer up, in `Tier::effective_config()` (`crates/spelunk-cli/src/capability.rs`), which synthesizes a `server_url` (and `project_id`) from `Tier::Server { url, auto_discovered: true }` when `cfg.server_url` is `None`.

The split is in **which commands call `effective_config()` before `open_memory_backend()`**:

| Command | Calls `effective_config()`? | Where memory goes when a local server is up and no team `server_url` is set |
| --- | --- | --- |
| `memory search` | **yes** (`memory/search.rs:24`) | **server.db** (via Remote backend) |
| `memory timeline` | **yes** (`memory/timeline.rs:19`) | **server.db** |
| `memory harvest` | **yes** (`memory/harvest.rs:28`) | **server.db** |
| `explore` | **yes** (`explore.rs:55`) | server.db (inference + memory) |
| `memory add` | **no** (`memory/add.rs:60`) | **memory.db** (local SQLite) |
| `memory list` | **no** (`memory/list.rs:13`) | **memory.db** |
| `context` | **no** (`context.rs:85`) | **memory.db** |
| `plumbing read-memory` | **no** (`plumbing/read_memory.rs:11`) | **memory.db** |

### The consequence: split-brain

With a local server running (the default v0.8.0 UX) and no team `server_url`:

- `spelunk memory add` writes to **`memory.db`**.
- `spelunk memory search` reads from **`server.db`** — and will **not** find the entry just added.
- `spelunk memory list` and `spelunk context` read from **`memory.db`** — and **will** find it.

So whether a freshly added decision is visible depends on which command you use to look for it. `memory search` and `context` disagree about what memory exists. This is the exact failure mode behind the 2026-06-11 SSE incident class: a decision recorded in one place, invisible from another. ADR-003 fixes *cross-project* invisibility; this ADR fixes *intra-project, intra-machine* invisibility caused by two stores for one project.

### Why this happened

The local server was introduced (v0.8.0) primarily as an **inference** backend — it bundles the native embedder so semantic search and LLM features work with zero external setup. The commands that need inference (`search`, `timeline`, `harvest`, `explore`) were wired through `effective_config()` so their embedding/LLM calls reach the server. As a side effect, those same commands also began routing their **memory CRUD** to the server, because `RemoteMemoryBackend` does both. The write path (`memory add`) and the non-inference read paths (`list`, `context`) were never moved, so they kept writing to and reading from local `memory.db`. The result is an accidental, undocumented split, not a designed one.

## Decision

**There is one canonical store for a project's memory, and it is the local SQLite `memory.db`.** The server is a cache / inference layer over it, never a second source of truth. Concretely:

### 1. `memory.db` is the system of record for the CLI tier

This is already the stated principle (decisions #76, #77: "local stays source of truth"). We make it true for *all* memory operations, not just writes. Every `spelunk memory` read and write — `add`, `list`, `search`, `timeline`, `context`, `harvest`, `read-memory` — resolves to the same `memory.db` for the active project as its store of record.

### 2. The auto-discovered local server stops being a memory backend; it stays an inference backend

Split the two roles that `Tier::Server` currently conflates:

- **Inference role** (embed / LLM / summaries): unchanged. Commands continue to reach the server via `ServerLlmClient` / `ServerEmbedClient` (`server_client.rs`) for embeddings and completions.
- **Memory-storage role**: an **auto-discovered** local server (`auto_discovered = true`) is **not** treated as a memory backend. `effective_config()` must populate `server_url` for **inference routing only**, and must **not** cause `open_memory_backend()` to select `RemoteMemoryBackend`.

This requires separating "where do I send inference requests" from "where does memory live." Today both ride on `cfg.server_url`. The fix is to stop overloading `server_url` for the auto-discovered case (see Implementation, below). A team/remote server set **explicitly** via config `server_url` keeps owning memory (that is the team-memory tier, decision #75) — only the *auto-discovered loopback* server is demoted to inference-only.

### 3. Selection rule (supersedes the implicit current behaviour)

The memory backend for a project is selected by this strict order, regardless of which command is running:

1. `backend_override == "git-notes"` → `GitNotesBackend` (explicit opt-in, unchanged).
2. **Explicit** `server_url` in config (team/remote server) → `RemoteMemoryBackend` (team-memory tier; memory lives on the shared server by the user's deliberate choice).
3. Otherwise → `LocalMemoryBackend` over the project's `memory.db`. **An auto-discovered loopback server does not change this.**

The distinguishing fact is `Tier::Server { auto_discovered }`: `auto_discovered = true` → inference-only, memory stays local; `auto_discovered = false` (operator-configured `server_url`) → memory on the server.

### 4. `memory.db` and `server.db` must never both be a project's authoritative store at once

For a project at CLI tier, `server.db` holds no authoritative memory rows for that project. If a local server has embeddings or a vector index it needs for `memory search`, those are derived artifacts populated from `memory.db` (via `memory push` / sync), not an independent record. The canonical text + metadata of every note lives in `memory.db`.

## How `memory search` keeps working without a second store

`memory search` needs (a) an embedding of the query and (b) a vector KNN over note embeddings. Under this decision:

- The **query embedding** still comes from the server (inference role) — unchanged.
- The **KNN** runs against the **local `memory.db`** using the same `sqlite-vec` `vec0` path that `LocalMemoryBackend` already uses for `search`/`search_hybrid` (the `MemoryBackend` trait already defines `search`, `search_text`, `search_hybrid` for local backends). The CLI embeds the query via the server, then performs the vector search locally against `memory.db`. No note text leaves the local store; only the query string is sent to the loopback embedder.

This is the same shape as decision #77 ("CLI sends text, server embeds, local stays source of truth"), applied consistently to *every* memory read rather than only some of them.

## Alternatives considered

- **Make `memory.db` a symlink/alias to `server.db`.** Rejected: couples the project-local store to a machine-global daemon file, breaks the per-project ownership model (each project's `.spelunk/memory.db` is self-contained and travels with the repo checkout), and makes `SPELUNK_NO_SERVER=1` / offline use lose access to memory written while the server was up.
- **Promote `server.db` to the canonical store and make `memory add` write to the server.** Rejected: contradicts decisions #75/#76/#77 (CLI tier must work with zero infrastructure and local-as-source-of-truth); makes the zero-infra tier depend on a running daemon for the most basic operation (recording a decision); and a machine-global `server.db` cannot represent per-project ownership without re-implementing the registry inside the server.
- **Two-way sync between `memory.db` and `server.db`.** Rejected: introduces a reconciliation/conflict problem (last-writer-wins, clock skew, supersede-edge ordering) for zero user benefit, since the local store is already authoritative and the server adds no durability the local file lacks.
- **Leave it as is and document the split.** Rejected outright — this is the split-brain Johan asked us to remove. "There should be one way of storing memory."

## What breaks / migration

- Any user who, since v0.8.0, recorded notes that landed in `server.db` (e.g. via `memory harvest` while a local server was running) has memory that `memory list` / `context` cannot see. A one-time **reconciliation** is needed: on first run after this change, if `~/.local/state/spelunk/server.db` contains memory rows for a project whose `memory.db` lacks them, surface them for import into `memory.db` (a `spelunk memory reconcile` / import step). This is a follow-up implementation task, scoped below — the ADR's job is to fix the steady state and not silently strip already-stored memory.
- `effective_config()` callers that relied on the side-effect of memory going to the server (`memory search`, `timeline`, `harvest`) change behaviour: their memory CRUD now targets `memory.db`. Their **inference** behaviour is unchanged. This is the intended fix, not a regression.
- The **team-memory tier** (operator-set `server_url`) is untouched: memory still lives on the shared server there, by deliberate configuration.

## Security note (Architect SAMM — Design / Secure Architecture)

This change *reduces* surface: note text for `memory search` no longer needs to be round-tripped to even a loopback server for storage; only the query embedding leaves the process boundary, and only to the local loopback embedder. No new network listener, no new trust boundary. The existing secret-scan gate (decision #115) sits on the `memory add` write path into `memory.db` and continues to protect the single canonical store. Update `docs/security/THREAT-MODEL.md` to reflect that a project's authoritative memory is `memory.db` only (no dual-store exfil surface via `server.db`).

## Implementation checklist (for implementer, pending Johan approval of this ADR)

> Architect does not write Rust impl/tests. This checklist is the contract.

- [ ] **Decouple inference routing from memory-store selection.** Introduce a distinct signal for "auto-discovered loopback server, inference-only" so it does not flow into `open_memory_backend()` as a memory `server_url`. Options for the implementer to choose between (document the choice in the PR):
  - (a) Add an `inference_url: Option<String>` to the effective `Config` (or a separate field) that `effective_config()` populates for `auto_discovered` servers, leaving `server_url` (the memory selector) `None`; `server_client.rs` reads `inference_url` (falling back to `server_url`); `open_memory_backend()` keeps reading only `server_url`. **Preferred** — keeps the memory selector single-purpose.
  - (b) Pass an explicit `memory_local_only: bool` into `open_memory_backend()` set true when the only server is `auto_discovered`, short-circuiting the `RemoteMemoryBackend` arm.
- [ ] **Route every memory command through the same local store** when the server is auto-discovered: `memory search`, `memory timeline`, `memory harvest` must select `LocalMemoryBackend` over `memory.db` for storage while still using the server for embeddings/LLM.
- [ ] **`memory search` local KNN path:** embed the query via the server, run the vector search against `memory.db` through the existing `MemoryBackend::search` / `search_hybrid` local implementations. Verify `search_text` (BM25) already runs locally (it does — no embedding needed).
- [ ] **Reconciliation/import** (`spelunk memory reconcile` or an import step in `memory list`/`context` first-run): detect memory rows in `~/.local/state/spelunk/server.db` for the active project that are absent from `memory.db`, and import them into `memory.db`. One-time, idempotent, dedupe by note id / content hash.
- [ ] **Docs:** update `CLAUDE.md` ("Optional: team memory server" + the inference section) and `docs/agent-guide.md` to state plainly: a project's memory lives in `memory.db`; a *team* `server_url` moves it to the shared server; an auto-discovered loopback server is inference-only and never a memory store.
- [ ] **THREAT-MODEL.md:** update the memory data-flow to show one authoritative store (`memory.db`) at the CLI tier.

### Tests

- [ ] With a loopback server running and **no** team `server_url`: `memory add` then `memory search` for the new note returns it (no split-brain). Currently this fails.
- [ ] With a loopback server running: `memory add` then `memory list` and `context` return the note (unchanged), and `memory search` returns the *same* note set (newly consistent).
- [ ] With an **explicit** team `server_url`: `memory add`/`list`/`search` all target the remote server (team-memory tier unchanged) — regression guard.
- [ ] `SPELUNK_NO_SERVER=1`: all memory commands operate on `memory.db`; no attempt to reach a server for storage.
- [ ] Reconcile/import: a `server.db` with a note absent from `memory.db` results in that note present in `memory.db` after the import step; running it twice is a no-op.

## Follow-ups

- Ops: confirm no production/team deployment relies on the auto-discovered loopback server holding authoritative memory (it should not — team memory is the explicit-`server_url` tier).
- Coordinate ordering with ADR-003: ADR-003's cross-project pass reads each linked project's `memory.db`. Once this ADR lands, "the project's memory store" is unambiguously `memory.db` for every project, which is exactly the assumption ADR-003 already encodes (`dep.db_path.with_file_name("memory.db")`). The two ADRs are mutually reinforcing; no conflict.
