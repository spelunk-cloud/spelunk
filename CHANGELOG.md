# Changelog

All notable changes to spelunk are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
spelunk uses [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

### Changed

- **Default memory backend is now `git-meta`** — `open_memory_backend()`
  resolves the backend in this order: an explicit `--backend` flag, then a
  configured `server_url` (remote), then a configured local embedder (SQLite),
  and finally `git-meta` as the zero-infrastructure default. Fresh installs no
  longer require a local embedder or SQLite database to record memory. The
  `--backend` flag now defaults to `auto`; pass `--backend sqlite` to opt back
  into the local SQLite backend with semantic search, or `--backend git-meta`
  to force it explicitly. (#283, #182)

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
  `escape_xml`, NDJSON, and history entry parsing.

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
  subcommands emit machine-readable NDJSON to stdout and use conventional exit
  codes (0 = ok, 1 = no results, 2 = error). All porcelain commands now accept
  `--format text|json|ndjson` for structured output in scripts and agents.
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
