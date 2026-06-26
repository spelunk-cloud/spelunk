# Changelog

All notable changes to spelunk are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
spelunk uses [Semantic Versioning](https://semver.org/).

---

## [Unreleased] — 0.9.0-dev

### Breaking changes — migration required

**Default embedder is now F2LLM-v2-330M via candle, 896-dim, GPU-accelerated on macOS.**

The bundled native embedder has switched from fastembed-rs / Nomic Embed Text v1.5
(768-dim, ONNX) to **codefuse-ai/F2LLM-v2-330M** (896-dim, Qwen3 decoder, safetensors)
served via the `candle` runtime. On macOS the prebuilt binary uses Metal GPU
acceleration; Linux falls back to CPU.

**Re-index required:** existing local indexes have 768-dim embeddings and will not
produce correct results against the 896-dim model. Run `spelunk index <project>`
after upgrading. The old embeddings table is automatically dropped and recreated
on first open.

**Model download on first run:** F2LLM-v2-330M weights (~650 MB) are downloaded
from Hugging Face Hub into `~/.local/share/spelunk/models/` on first `spelunk-server`
startup. Subsequent starts use the cached weights with no network access.

---

## [Unreleased] — 0.8.4-dev

### Added

- **`spelunk login` / `spelunk org switch` / `spelunk logout`** — browser-based
  device login for spelunk.cloud with short-lived, auto-refreshing tokens.
  `spelunk login` prints a verification URL and a short user code; open the URL
  in a browser, enter the code, and approve the sign-in. Multi-org accounts
  select their organization on the browser-hosted approval page. On success,
  tokens are stored in the `[auth]` table of
  `~/.config/spelunk/config.toml` (mode `0600`). Polls for approval with
  back-off on `authorization_pending` / `slow_down`. The tokens are refreshed
  transparently on expiry, so you stay logged in without re-running `login`.
  `spelunk login --org <slug>` logs in and then re-scopes to the named org.
  `spelunk org switch <slug|uuid>` re-scopes an existing session without a new
  device login. `spelunk logout` clears the stored credentials. `--cloud-url`
  (or `SPELUNK_CLOUD_URL`) overrides the default `https://api.spelunk.cloud`
  endpoint. Existing setups using a static `server_key` / `SPELUNK_SERVER_KEY`
  keep working until the next `spelunk login`, and `SPELUNK_SERVER_KEY` still
  takes precedence for CI.

- **Two-way memory sync with spelunk.cloud (`spelunk sync` / `spelunk memory
  sync`).** When a team server is configured, `spelunk sync` (top-level alias
  for `spelunk memory sync`) now performs a real two-way sync: it pushes local
  memory entries the cloud has not seen and pulls remote entries into the local
  `memory.db`, applying changes keep-both so concurrent edits on multiple
  machines are preserved. A new `spelunk memory pull` does a one-way delta pull.
  Sync is identity-keyed on a time-ordered UUID carried by each entry, so it is
  idempotent (re-running never duplicates) and drift-free across machine clocks;
  archived entries propagate as tombstones. (ADR-037 P1, #425)

- **Sync modes (`mode = offline | local_first | cloud_first`).** A new `mode`
  config field (and `SPELUNK_MODE` env override) controls how the CLI reconciles
  local and cloud memory. The default preserves existing behaviour: with no
  `server_url` the CLI is `offline`; with a `server_url` set it is `local_first`.
  `SPELUNK_NO_SERVER=1` remains a hard kill-switch. (ADR-037 P1, #425)

### Changed

- **Cloud auth now uses short-lived, auto-refreshing tokens instead of a
  non-expiring key.** `spelunk login` stores access/refresh tokens under the
  `[auth]` table of the config; requests send the access token as a bearer
  credential and the CLI refreshes it transparently on expiry, retrying the
  original request once. Bearer precedence is `SPELUNK_SERVER_KEY` (env) > stored
  `[auth]` access token > legacy bare `server_key`, so existing `server_key`
  users keep working with no flag-day and `SPELUNK_SERVER_KEY` still overrides
  for CI and headless use.

- **Cloud project slug auto-resolves to its server UUID.** When a team
  `server_url` routes projects by an internal UUID, a human `project_id` slug is
  now resolved to that UUID on first use via `GET /v1/projects` and cached in
  `.spelunk/cloud-project-id.lock`; a raw-UUID `project_id` is used directly, and
  a loopback/unset server is left untouched. The cache is invalidated
  automatically if the slug changes, and `SPELUNK_NO_SLUG_CACHE=1` forces a fresh
  lookup. This makes the human-readable `project_id` work transparently against
  cloud-api routing. (ADR-005, #428)

### Dependencies

- `tower-http` 0.6.11 → 0.7.0 (#431)
- `actions/checkout` 6 → 7 (CI) (#432)
- Refreshed `Cargo.lock` to latest semver-compatible versions; no new advisories
  (#433)

## [0.8.3] — 2026-06-17

### Changed

- **Server instance_id now uses UUID v7 instead of v4.** The persistent instance ID (returned by `/v1/health` and stored in the server database) is now a time-ordered UUID v7 minted by the `uuid` crate (`Uuid::now_v7`) and persisted verbatim, so the high 48 bits encode the millisecond Unix timestamp of creation and the value is stable for the life of the database. It remains a standard 36-char UUID string. (#416)

- **Generate UUIDs with the `uuid` crate instead of hand-rolled helpers.** The bespoke `format_uuid_v7()` byte-formatter in spelunk-server and the `query_nonce_hex()` helper in the spelunk-cli server client (the latter previously the misnamed `uuid_v4_hex()`) have both been removed; the server instance id and the synthetic `query:<id>` embed chunk id are now produced by `Uuid::now_v7()`. (#416)

### Security

- **`explore` `read_file` confined to indexed files within the project root.**
  The `explore` tool loop previously read whatever path the LLM supplied, so
  adversarial instructions embedded in indexed source content could steer it
  into reading an arbitrary file the process could access (for example
  `~/.ssh/id_rsa` or a `../../etc/passwd` traversal) and returning the contents
  in the answer or step log. `read_file` now resolves every requested path
  against an allow-list: absolute, drive, UNC, and NUL inputs are rejected, `..`
  escapes are rejected lexically, the path must match an indexed file in the
  `files` table, and the canonicalized target must stay under the canonical
  project root (symlink backstop). A denied read is a recoverable tool result
  that echoes only the caller-supplied path, never a resolved path or file
  contents, and no longer aborts the session. (#403)
- **Storage SQL `IN (...)` queries are now fully parameterised and bind-limit
  chunked.** The four list-based query methods (`chunks_by_ids`,
  `graph_neighbor_chunks`, `mention_edges_for_chunks`, `chunks_mentioning_symbols`)
  previously assembled their `IN (...)` placeholder list at runtime with `format!`.
  No caller value was ever interpolated into the SQL text, so there was no active
  injection vector, but the hand-built placeholder construction was a latent
  hazard. A shared `placeholders(n)` helper now emits the placeholder list and all
  values are bound positionally via rusqlite params, so no caller-supplied id,
  name, or symbol can reach the SQL string. Each method also chunks its input at
  the SQLite bind-parameter limit (halved for the two-clause `graph_neighbor_chunks`)
  and merges results with unchanged semantics, removing the prior cap on input
  list size. Internal change only; no user-facing behaviour change.
  ([#405](https://github.com/spelunk-cloud/spelunk/issues/405))

### Internal

- Repaired the fuzz crate after the three-crate workspace migration and suppressed an upstream tree-sitter-sequel LeakSanitizer false positive so the fuzz CI job runs clean. ([#417](https://github.com/spelunk-cloud/spelunk/pull/417), [#418](https://github.com/spelunk-cloud/spelunk/pull/418))

## [0.8.2] — 2026-06-15

### Security

- **gix/gitoxide Dependabot alerts verified resolved (P1-2).** All 7 open Dependabot
  security alerts (6 high, 1 medium) for gix-related crates were already cleared by
  previous bumps (PRs #307, #327, #334). Cargo.lock contains patched versions:
  gix 0.84.0 (>=0.83.0), gix-fs 0.21.2 (>=0.21.1), gix-pack 0.71.0 (>=0.69.0),
  gix-transport 0.57.1 (>=0.56.0). `cargo audit` returns zero vulnerabilities
  (exit 0); the unmaintained `paste` crate surfaces as a non-failing warning
  (RUSTSEC-2024-0436), and the one advisory suppressed in audit.toml is
  RUSTSEC-2026-0097 (rand 0.10.0 via lopdf, no patched release yet).
  Affected advisories: GHSA-fr8x-3vfx-f45h, GHSA-pg4w-g64p-qwhj, GHSA-f26g-jm89-4g65,
  GHSA-p3hw-mv63-rf9w, GHSA-f89h-2fjh-2r9q, GHSA-x494-mj8g-cj27, GHSA-9857-6mw7-fq2m.

- **`spelunk memory add` blocks the entire write on secret detection.** When a
  secret is detected at input time, both the SQLite backend write and the
  git-notes write are now aborted — no partial write occurs. Previously only
  the git-notes path was guarded; the SQLite path could still persist a
  credential-containing entry. (#344)

### Added

- **Cross-project memory visibility (`spelunk memory search|list|context`).** When
  projects are linked with `spelunk link`, `memory search`, `memory list`, and
  `context` now also query each linked project's memory store and surface
  `locked` or `cross-project`-tagged `decision` and `requirement` entries
  alongside local results. Each cross-project result is tagged with its source
  project (`[from: <project>]` in text mode; `source_project` /
  `source_project_path` fields in JSON) so conflicting decisions remain
  attributable. `handoff` and `question` entries remain strictly project-local.
  (ADR-003)

- **`--local-only` flag for `memory search`, `memory list`, and `context`.**
  Suppresses the cross-project dep pass and queries only the primary project's
  memory store -- matching the existing `spelunk search --local-only` behaviour.
  (ADR-003)

- **`spelunk memory reconcile`** — imports notes from a local `spelunk-server`
  database (`server.db`) into the project's `memory.db` by content-hash dedup,
  without running the server. Reads `server.db` in read-only mode; never writes
  to it. Flags: `--source-db <path>` (override the default source path
  `~/.local/state/spelunk/server.db`), `--dry-run` (report candidates without
  importing), `--all-projects` (reconcile every project slug found in
  `server.db`), and `--format json` (machine-readable summary). Exits with code
  0 when there is nothing to import. (#391, ADR-004 follow-up)

### Fixed

- **SQLite LIKE queries now escape metacharacters in file paths and symbol names.** File
  paths and symbol names containing `%` or `_` were causing over-matching in
  `file_paths_under`, `chunks_for_file`, `symbol_history`, and `stale_specs` queries.
  An `escape_like()` helper now escapes these characters before binding, with
  `ESCAPE '\\'` on the SQL clause. ([#406](https://github.com/spelunk-cloud/spelunk/issues/406))
- **Batch summariser: use per-batch UUID delimiter to prevent chunk-content
  spoofing.** The batch summariser was using a static `===CHUNK {id}===` delimiter
  between chunks in the LLM prompt. Source code chunks whose content contained
  that string could spoof chunk boundaries and confuse the model. Now uses a
  per-batch UUID: `===CHUNK-{uuid}={id}===`. (#404)

- **`spelunk memory watch --help` now references `server_url` correctly.** The
  subcommand doc-comment previously said `requires memory_server_url` (the
  deprecated alias); corrected to `requires server_url`.
  ([#400](https://github.com/spelunk-cloud/spelunk/issues/400))

- **`explore` now appears in `spelunk --help`.** The `explore` subcommand was
  inadvertently hidden from the top-level help output; the hide attribute has
  been removed so users can discover it alongside the other subcommands.
  ([#400](https://github.com/spelunk-cloud/spelunk/issues/400))

### Dependencies

- `openssl` 0.10.80 → 0.10.81 (#408)
- `openssl-sys` 0.9.116 → 0.9.117 (#410)
- `regex` 1.12.3 → 1.12.4 (#409)

---

## [0.8.1] — 2026-06-10

### Fixed

- **`spelunk search` honors auto-discovered loopback server.** Explicit
  `--mode semantic`/`--mode hybrid` no longer error with "requires
  spelunk-server", and `--mode auto` no longer silently falls back to
  ast-grep, when no `server_url` is configured but a local `spelunk-server`
  was auto-discovered via the loopback probe (the default v0.8.0 UX).

- **Native embedder: fixed memory spike, CPU saturation, and index timeout.**
  Indexing large projects no longer triggers a ~20 GB memory spike, ~750%
  CPU usage across 31 threads, or HTTP timeouts during the embed phase.
  Adds a new `--embed-threads` CLI arg (default 4, env
  `SPELUNK_EMBED_THREADS`). Verified: 124-file / 1330-chunk index completes
  in 7m30s with stable ~3.5 GB memory and ~350-400% CPU.

- **Native embedder: reduced CoreML activation footprint and added compiled-model
  cache** for hardware EP builds (`embed-coreml` / `embed-xnnpack` /
  `embed-directml`), cutting peak memory from ~4 GB to ~1 GB and avoiding
  CoreML recompilation on every server start. These hardware EP features
  remain experimental and are not recommended over the default CPU EP — see
  `docs/server.md`.

### Security

- **Auto-spawned `spelunk-server` now binds to `127.0.0.1` only.** Previously
  the server started by `spelunk init` / `ensure_server_running` defaulted to
  `0.0.0.0`, making the unauthenticated local server LAN-reachable.
  (THREAT-MODEL req #9, decision #88)

### Added

- **`spelunk status`/`check --format json`** now include a `memory_backend`
  field (`"sqlite"`, `"remote"`, or `"git-notes"`); `spelunk status` text mode
  shows a "Memory backend: <kind>" line. (#308)

### Changed

- **NDJSON terminology renamed to JSONL** throughout the CLI, docs, and tests.
  The `--format` flag value `ndjson` is now `jsonl` for `search`, `graph`, and
  `memory` commands. (#348)

- **Internal refactor:** `storage::remote` and `storage::git_notes` split into
  module directories to stay under the 400-line file limit. No public API
  changes.

- **Homebrew tap moved to a separate repo** (`spelunk-cloud/homebrew-spelunk`);
  the release workflow now publishes the formula there directly.

---

## [0.8.0] — 2026-06-08

### Breaking changes — migration required

**All AI inference commands now route through `spelunk-server`.**

The following commands previously called LM Studio (or another
OpenAI-compatible endpoint) directly via `api_base_url`. They now require a
running `spelunk-server` reachable at `server_url` in your config:

| Command | Previously needed | Now needs |
|---|---|---|
| `spelunk explore` | `api_base_url` + `llm_model` | `server_url` |
| `spelunk search` (semantic/hybrid) | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory search` (semantic) | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory timeline` | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory add` (auto-embed) | `api_base_url` + `embedding_model` | `server_url` (optional, degrades gracefully) |
| `spelunk index` (embed phase) | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk index` (summaries) | `api_base_url` + `llm_model` | `server_url` |
| `spelunk plumbing embed` | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory harvest` | `api_base_url` + `llm_model` | `server_url` (unchanged since #310) |

**Migrating from `lm_studio_url` / `api_base_url`:**

If you previously ran a local LM Studio and set `api_base_url` in your config,
you now need to run `spelunk-server` in front of it:

```toml
# ~/.config/spelunk/config.toml

# Old config (no longer used for inference):
# api_base_url = "http://127.0.0.1:1234"

# New config:
server_url = "http://127.0.0.1:7777"   # spelunk-server address
project_id = "your-org/your-project"   # required when server_url is set
```

Start `spelunk-server` and point it at your LM Studio instance:

```sh
spelunk-server \
  --embedding-url http://127.0.0.1:1234 \
  --embedding-model text-embedding-embeddinggemma-300m-qat \
  --llm-url http://127.0.0.1:1234 \
  --llm-model google/gemma-3n-e4b \
  --port 7777
```

Commands that do **not** need inference (parse, graph, FTS search, status,
memory list/show/archive) continue to work offline without `server_url`.

### Changed

- **`spelunk-core` no longer contains embedding or LLM implementations.**
  `OpenAiCompatEmbedder`, `OpenAiCompatLlm`, and `backends.rs` have been
  removed from `spelunk-core`. The `EmbeddingBackend` and `LlmBackend` traits
  remain in `spelunk-core` for use by `spelunk-server`'s `AppState`. (#260, #312)

- **Capability module moved from `spelunk-core` to `spelunk-cli`.** The tier
  detection logic (`get_tier`, `require_tier1`) is now internal to the CLI
  binary. Nothing outside spelunk-cli should depend on it. (#312)

---

## [0.7.1] — 2026-05-27

### Added

- **`spelunk-server` HTTP API** — Axum-based REST server with AuthProvider
  trait, `/v1/embed`, `/v1/explore`, `/v1/plan` endpoints, and an OpenAPI spec
  committed alongside the binary. Server-side embedding is optional; pass
  `SPELUNK_EMBEDDING_URL` to enable it. Prompt-injection patterns are rejected
  server-side before storage. (#261, #221, #222)

- **`spelunk status --format json`** — stable machine-readable schema for
  status output, suitable for CI dashboards and agent health checks. (#269)

- **Heuristic convention extraction** — `spelunk index` now detects and stores
  project conventions (naming patterns, async style, test coverage, doc
  coverage) derived from the AST. Results are surfaced in `spelunk context`
  output. (#268)

- **Compatibility tier model** — `spelunk check` reports a capability tier
  (Local / Embedded / Full) so agents can adapt their strategy to the available
  inference backend at runtime. (#259)

- **`spelunk graph --live`** — passes the query to ast-grep as a fallback when
  the indexed call graph has no results, giving live symbol resolution for
  unindexed or recently changed code. (#216)

### Changed

- **3-crate Cargo workspace** — the codebase is now split into `spelunk-core`
  (library), `spelunk-cli` (binary), and `spelunk-server` (binary + lib) under
  a shared workspace root. `CLAUDE.md` and `README.md` updated accordingly.
  (#220)

- **`gix` status API** — subprocess calls to `git status` replaced with
  `gix::status` API, removing a shell dependency and improving reliability
  inside IDE integrations. (#215)

- **`spelunk explore` now requires a configured server** — the command is gated
  behind the Tier 2/3 capability check (`server_url` must be set and reachable).
  The previous check for `llm_model` has been removed in line with decision #47
  (no LLM inference in the CLI without a server). Run `spelunk status` for
  guidance if the command is unavailable.

### Fixed

- `spelunk-server` OpenAPI spec gaps: `SearchRequest` missing `text` field,
  JSON error shapes aligned to `application/json` responses, CI step added to
  gate spec drift. (#288)

- `.spelunk` symlink replaced with runtime worktree-root resolution, fixing an
  infinite-symlink issue when indexing inside a git worktree. (#266)

- `spelunk memory harvest` now swallows per-entry errors and continues rather
  than aborting the entire run on a single bad entry. (#270)

### Dependencies

- `serde_json` 1.0.149 → 1.0.150
- `tree-sitter` 0.26.8 → 0.26.9
- `tower-http` 0.6.10 → 0.6.11

---

## [0.7.0] — 2026-05-17

### Added

- **`spelunk context`** — new agent session entry point command that surfaces
  index health, recent memory, and open questions in a single structured
  output, making it easier for AI agents to orient at the start of a session.

- **`spelunk memory harvest --source entire`** — mines Entire.io checkpoint
  files (stored on the `refs/entire/checkpoints/v1` branch) for decisions,
  notes, and requirements. The structured `Summary` in each checkpoint is used
  directly; an LLM fallback is used only for checkpoints that lack one. Secret
  scanning is applied on all paths so credentials are never stored.

- **`GitMetaBackend`** — second memory backend backed by `git-meta-lib`, available
  alongside the existing git-notes and server backends.

- **`git-notes` memory backend** (`GitNotesBackend`) — store and retrieve memory
  entries using git's native notes mechanism, with no server or external database
  required. Supports optional embedding for semantic memory search. `NoteRecord`
  now carries a `schema_version` field for forward-compatible detection of future
  record formats.

- **Benchmark suite** (`bench/`) — five benchmarks now ship with the repo to
  measure and document retrieval quality and performance: Decision Archaeology
  (memory recall), Cross-Session Handoff (agent continuity), SWE-bench
  (patch resolution rate via Docker harness), Code-Graph (grep vs search vs
  graph), and Perf-at-Scale (indexing and search latency at 50k–500k LOC).
  A benchmarking report (`tmp/benchmarking-report.md`) summarises current
  results.

### Changed

- **Removed legacy commands** — `ask`, `plan`, `spec`, `snapshot`, `history`,
  and `verify` subcommands have been removed. The core spelunk workflow is now
  index-free by default; docs and SKILL.md have been updated to reflect this.

- **`gix::discover` replaces `git rev-parse`** — subprocess calls to
  `git rev-parse` for repository discovery are replaced with `gix::discover`,
  removing a shell dependency.

- **Dependency updates** — `gix` bumped to 0.83.0 (fixes worktree exclude
  handling so `.gitignore` rules are correctly respected in git worktrees);
  `git-meta-lib` bumped to 0.1.10 with API adaptation.

### Fixed

- `GitNotesBackend` unsupported methods now return a typed `BackendUnsupported`
  error instead of panicking.

- `GitNotesBackend` list capped at 500 entries, preventing an O(n) hang on
  repositories with many notes.

- `spelunk index` no longer creates a self-referential `.spelunk` symlink when
  run inside a git worktree checked out at the same path as the main repo.

### Security

- Server audit checklist (`docs/security/`) added as groundwork for the upcoming
  v1.0 server release.

---

## [0.6.0] — 2026-04-26

### Added

- **LinearRAG two-stage retrieval** — a new graph-diffusion retrieval algorithm
  combining personalised PageRank with full-text pre-filtering. Now the default
  for `spelunk search`; multi-hop recall is +33.5 % over the previous baseline
  at ~1.9× median latency. Compound indexes and in-memory Stage 1 propagation
  keep latency within acceptable bounds.

- **`.spelunkignore` files** — place a `.spelunkignore` file anywhere in your
  project tree to exclude paths from indexing, using the same format as
  `.gitignore`.

- **`antipattern` memory kind + `spelunk memory failures`** — record failure
  patterns and anti-patterns as first-class memory entries. `spelunk memory
  failures` lists them; `spelunk memory harvest` can extract them from session
  history.

- **Expanded fuzzer coverage** — fuzzer targets now cover secrets, chunker,
  `escape_xml`, JSONL, and history entry parsing.

### Fixed

- OOM guard added to the parser; `doc_comment` nodes are skipped during AST
  walking to avoid memory exhaustion on files with large doc blocks.

- `spelunk check` was made fully async, fixing a panic caused by calling
  `block_on` inside an existing async context.

- PageRank dangling-node indices are now precomputed before power iterations,
  improving performance on sparse graphs.

---

## [0.5.0] — 2026-04-21

### Added

- **Unix plumbing/porcelain architecture** — 8 new `spelunk spelunk` plumbing
  subcommands emit machine-readable JSONL to stdout and use conventional exit
  codes (0 = ok, 1 = no results, 2 = error). All porcelain commands now accept
  `--format text|json|jsonl` for structured output in scripts and agents.
  Plumbing commands: `cat-chunks`, `embed`, `graph-edges`, `hash-file`, `knn`,
  `ls-files`, `parse-file`, `read-memory`.

- **`spelunk memory harvest --source claude-code`** — mines Claude Code session
  history files (`.claude/projects/*/sessions/*.jsonl`) for decisions, notes,
  and requirements; deduplicates against already-stored entries; stores the
  results directly in the memory index.

- **`intent` memory kind** — agents record work-in-progress intent entries so
  collaborating agents (and humans) can see what is actively being changed.
  `spelunk check` now shows active agent sessions alongside the index health
  summary, and warns when any intent's linked files overlap with files recently
  modified in the current worktree.

- **Server-side conflict detection** — `spelunk-server` runs a KNN similarity
  search before storing each new memory entry; entries that closely contradict
  an existing active entry are flagged with a `contradicts` edge and the HTTP
  response includes a `409 Conflict` status with the conflicting entry IDs.
  A `--conflict-threshold` flag controls the cosine-distance trigger.

- **`spelunk memory since` / `spelunk memory watch`** — incremental memory feed
  (`since`) and a long-running SSE stream (`watch`) for agents that want to be
  notified of new memory entries in real time. _(coming soon in 0.5.x — not
  yet merged as of this release)_

- **Benchmark scripts** (`bench/`) for evaluating search quality across
  indexing configurations.

### Changed

- `--format text|json` standardised across all porcelain commands (`ask`,
  `explore`, `search`, `graph`, `memory list`, `memory search`). The legacy
  `--json` flag is kept as a hidden deprecated alias.

- `storage/memory.rs` split into focused sub-modules
  (`storage/memory/`, `storage/db/`) to reduce file size and improve
  navigability, as part of the broader Unix-architecture refactor.

### Fixed / Security

- **XML escaping in LLM prompts** — spec titles and paths interpolated into
  `<spec_context>` blocks are now escaped with `escape_xml()`, closing a
  prompt-injection vector.

- **Expanded secret scanner** — `src/indexer/secrets.rs` now recognises
  OpenAI, Anthropic, and Stripe API keys; npm automation tokens; and database
  connection URLs containing inline credentials. Patterns compile once via
  `OnceLock`.

- **Atomic memory transactions** — `NoteStore` archive and supersede operations
  now run inside a single SQLite transaction; partial writes on crash are no
  longer possible.

- Resolved all security-audit findings from `cargo audit` (#136, #137, #138,
  #145) by upgrading affected dependency versions.

---

## [0.4.1] — 2026-03-21

Initial public release.
