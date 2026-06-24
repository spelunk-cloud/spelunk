# CLAUDE.md — spelunk

Developer guide for AI agents (and humans) working on this codebase.

---

## Agent workflow — use spelunk on this codebase

This project is indexed with spelunk. Use it — don't just use Read/Grep/Glob.

**At the start of every session:**
```bash
spelunk context                                   # pull prior decisions, handoffs, questions, requirements
spelunk check                                     # verify index is fresh (only if indexed)
```

**Before reading any file, search first:**
```bash
spelunk graph <symbol>                            # trace callers/callees (always works)
spelunk search "<topic>" --mode text              # full-text search (always works)
spelunk search "<topic>"                          # semantic search (if indexed + server running)
```

spelunk retrieves context — you synthesise the answer.

**Store decisions as you make them** — don't wait until the end:
```bash
spelunk memory add --kind decision --title "..." --body "why, what alternatives, what breaks"
spelunk memory add --kind requirement --title "..." --body "..."   # when user states a constraint
spelunk memory add --kind note --title "..."                       # surprising/non-obvious facts
```

**At the end of every session:**
```bash
spelunk memory add --kind handoff --title "Handoff: <summary>" --body "what's done, what's next, open questions"
# Optional: re-index if you've indexed the project
spelunk index .
```

Full reference: `SKILL.md` and `docs/agent-guide.md`.

---

## What This Project Is

`spelunk` is a Rust CLI and context retrieval engine for AI agents.

**Built-in (zero infrastructure):** git-notes memory, full-text search, code graph (AST + call edges), tree-sitter chunking. Works immediately with no setup.

**Semantic search via spelunk-server:** from v0.9.0 the default UX runs a local `spelunk-server` (auto-bound on `127.0.0.1`). The server bundles a native embedder (codefuse-ai/F2LLM-v2-330M, 896-dim, candle runtime, Metal/GPU on macOS) — no external embedding endpoint required. Semantic search, `spelunk explore`, `spelunk memory harvest`, and LLM summaries all route through the server's inference endpoints; the CLI talks to it via `server_client.rs`. Manage the daemon with `spelunk server start|stop|status|logs`. This **auto-discovered loopback server is an inference backend only** — it embeds queries and runs LLM calls, but it is **never** a memory store. A project's memory always lives in its local `memory.db`; the loopback server holds no authoritative memory.

**Optional: team memory server** (`server_url` *explicitly* set in config, pointing at a shared instance): share memory (decisions, requirements) across a team. Setting an explicit `server_url` is the **only** way memory moves off the local `memory.db` — it relocates the store of record to the shared server. Each developer's code stays local. (Note the distinction: an auto-discovered loopback server provides inference and never owns memory; an explicit team `server_url` does own memory. They must not be conflated.) When a team server routes projects by an internal UUID, a human `project_id` slug is auto-resolved to that UUID on first use and cached in `.spelunk/cloud-project-id.lock` (see ADR-005); a raw UUID `project_id` is used directly.

You search with spelunk, then reason over the results yourself.

---

## Workspace Structure

This is a Cargo workspace with three crates:

```
Cargo.toml                    — workspace root; [workspace.dependencies] for shared versions

crates/
  spelunk-core/               — library: storage, indexer, embeddings, LLM, search, config, registry
  spelunk-cli/                — `spelunk` binary; depends on spelunk-core
  spelunk-server/             — `spelunk-server` binary + lib; depends on spelunk-core
```

## Module Map

### spelunk-core (`crates/spelunk-core/src/`)

```
lib.rs           — crate root; re-exports public modules
error.rs         — SpelunkError enum
config.rs        — Config struct; load from ~/.config/spelunk/config.toml
utils/
  mod.rs         — strip_ansi(), misc helpers
  dates.rs       — date parsing helpers
registry.rs      — global project registry (~/.config/spelunk/registry.db)

conventions/
  mod.rs         — ConventionRecord type; re-exports ConventionExtractor
  extractor.rs   — ConventionExtractor: heuristic AST pass over stored chunks
  rules/
    mod.rs       — per-language rule dispatch
    generic.rs   — language-agnostic convention rules
    rust.rs      — Rust-specific convention rules
    typescript.rs — TypeScript-specific convention rules

embeddings/
  mod.rs         — EmbeddingBackend trait, vec_to_blob/blob_to_vec helpers

llm/
  mod.rs         — LlmBackend trait, Message struct, Token type

indexer/
  mod.rs         — re-exports Chunk, ChunkKind, SourceParser
  chunker.rs     — Chunk / ChunkKind structs; sliding_window fallback
  docparser.rs   — document-level parsing helpers
  pagerank.rs    — PageRank over the code graph
  pdf.rs         — PDF text extraction
  secrets.rs     — contains_secret(): regex scanner, drops credential chunks
  summariser.rs  — LLM-based chunk summarisation
  graph/
    mod.rs       — re-exports EdgeExtractor
    edges.rs     — EdgeExtractor: import/call/extends edges via tree-sitter
    builtins.rs  — built-in symbol skip-list
  parser/
    mod.rs       — SourceParser; detect_language; SUPPORTED_LANGUAGES
    text.rs      — plain-text / sliding-window parser
    ts_walker.rs — tree-sitter AST walker

storage/
  mod.rs         — re-exports Database
  db.rs          — Database struct; open/migrate; connection setup
  files.rs       — file record CRUD (insert, lookup, delete)
  chunks.rs      — chunk CRUD (insert, fetch, delete by file)
  conventions.rs — conventions table CRUD (no dependency on conventions/)
  search.rs      — KNN search queries against sqlite-vec
  graph.rs       — graph_edges CRUD
  snapshots.rs   — snapshot save/restore
  specs.rs       — spec record CRUD
  stats.rs       — aggregate statistics queries
  note_record.rs — NoteRecord struct (memory entry)
  git_notes/
    mod.rs         — GitNotesBackend struct + helpers; append_to_git_notes free function
    backend_impl.rs — MemoryBackend trait impl for GitNotesBackend
  memory/
    mod.rs       — NoteStore: memory entries CRUD + list_filtered
    edges.rs     — memory relationship edges CRUD
    notes.rs     — note insert/fetch/delete
    search.rs    — memory FTS + semantic search
    tests.rs     — integration tests for NoteStore
  backend.rs     — StorageBackend trait (local vs remote)
  remote/
    mod.rs         — RemoteMemoryBackend struct + URL helpers + MemoryBackend impl
    wire_types.rs  — HTTP request/response structs (AddNoteRequest, NoteResponse, etc.)
    tests.rs       — #[cfg(test)] tests for URL encoding and search wire format

search/
  mod.rs         — SearchResult struct
  rag.rs         — RagPipeline<E,L>: search + ask (dead code, kept for future)
  explore.rs     — interactive exploration pipeline
  tokens.rs      — token-budget helpers
  tools.rs       — tool-call helpers for LLM search

migrations/  (crates/spelunk-core/migrations/)
  001_initial.sql – 018_graph_edges_compound_idx.sql — incremental DB schema
```

### spelunk-cli (`crates/spelunk-cli/src/`)

```
main.rs          — entry point: parse CLI, dispatch to commands
capability.rs    — Tier 0/1 capability detection (server reachable probe, cached per-process)
server_client.rs — ServerLlmClient + ServerEmbedClient: HTTP clients for spelunk-server inference endpoints

cli/
  mod.rs         — clap structs (Cli, Command, *Args)
  cmd/
    mod.rs       — re-exports one pub fn per subcommand
    check.rs     — `spelunk check` handler
    context.rs   — `spelunk context` handler (agent session entry point)
    explore.rs   — `spelunk explore` handler
    graph.rs     — `spelunk graph` handler
    helpers.rs   — shared output / progress helpers
    hooks.rs     — `spelunk hooks` handler
    init.rs      — `spelunk init` handler
    link.rs      — `spelunk link/unlink/autoclean` handlers
    links.rs     — `spelunk links` handler
    misc.rs      — `spelunk chunks` / `spelunk languages` handlers
    search.rs    — `spelunk search` handler
    server.rs    — `spelunk server start/stop/status/logs` daemon management
    status.rs    — `spelunk status` handler
    ui.rs        — TUI helpers (private)
    index/
      mod.rs         — `spelunk index` entry point
      embed_phase.rs — embedding phase of indexing
      mentions.rs    — mention stopword filter used during indexing
      parse_phase.rs — parse/chunk phase of indexing
      summaries.rs   — AI summary generation during index
      worktree.rs    — git worktree handling for index
    memory/
      mod.rs          — `spelunk memory` dispatch
      add.rs          — memory add subcommand
      archive.rs      — memory archive subcommand
      failures.rs     — `spelunk memory failures` handler
      graph_cmd.rs    — memory graph subcommand
      harvest.rs      — memory harvest (LLM extraction) entry point
      harvest_claude.rs — harvest from ~/.claude/history.jsonl (Claude Code sessions)
      list.rs         — memory list subcommand
      push.rs         — memory push subcommand
      reconcile.rs    — memory reconcile subcommand (import from server.db)
      search.rs       — memory search subcommand
      show.rs         — memory show subcommand
      since.rs        — `spelunk memory since` handler
      supersede.rs    — memory supersede subcommand
      timeline.rs     — memory timeline subcommand
      watch.rs        — `spelunk memory watch`: SSE stream from spelunk-server
    plumbing/
      mod.rs               — PlumbingArgs/PlumbingCommand; dispatch; exit-2 on error
      cat_chunks.rs        — emit indexed chunks for a file as JSONL
      embed_cmd.rs         — read stdin lines, emit embedding vectors as JSONL
      graph_edges.rs       — emit code graph edges as JSONL
      hash_file.rs         — blake3 hash a file; check index currency
      knn.rs               — KNN vector search, JSONL output
      ls_files.rs          — list indexed files as JSONL; exit 1 if no results
      parse_file.rs        — parse a file and emit chunks as JSONL (no DB write)
      read_conventions.rs  — emit stored convention records as JSONL
      read_memory.rs       — emit memory entries as JSONL
```

### spelunk-server (`crates/spelunk-server/src/`)

```
main.rs            — entry point: parse args, register sqlite-vec, start Axum server
lib.rs             — AppState, router, auth_middleware, AppError, ApiDoc (utoipa)
db.rs              — ServerDb: SQLite schema, memory CRUD, KNN search, embedding dim guard
handlers.rs        — Axum route handlers for all /v1/ endpoints
embedder_native.rs — native embedder (F2LLM-v2-330M via candle, 896-dim, Metal/GPU on macOS; `embed-native` feature)

migrations/  (crates/spelunk-server/migrations/)
  server_001.sql — projects + server memory schema
  server_002.sql — server memory FTS
```

---

## Inference Backend

All AI inference goes through **spelunk-server**. The CLI calls the server via
`ServerLlmClient` and `ServerEmbedClient` in `crates/spelunk-cli/src/server_client.rs`
— these are the only places in spelunk-cli that issue AI inference requests.

`spelunk-core` defines the `EmbeddingBackend` and `LlmBackend` traits
(`embeddings/mod.rs`, `llm/mod.rs`) but ships **no concrete implementations**.
Concrete backends live in spelunk-server (not in this repo).

`capability.rs` probes server availability at startup and exposes a `Tier`
enum so commands degrade gracefully when no server is configured.

**Inference vs. memory storage are separate concerns.** Reaching the server for inference (`ServerLlmClient` / `ServerEmbedClient`) does **not** mean memory is stored there. For an auto-discovered loopback server, memory CRUD (`add`, `list`, `search`, `timeline`, `context`, `harvest`, `read-memory`) resolves to the project's local `memory.db`; the server is used only to embed the query for `memory search`, with the vector KNN run locally against `memory.db`. Memory lives on a server **only** when an explicit team `server_url` is configured. See `docs/adr/004-unified-memory-storage.md`.

---

## Key Design Decisions

### Chunking strategy
Tree-sitter extracts **named semantic nodes** (functions, structs, impls, etc.)
rather than naive line splits. Sliding-window (120 lines, 15-line overlap) is
the fallback for unsupported file types. Markdown uses ATX heading-based
chunking (each `# Heading` + body = one `ChunkKind::Section`).

### Embedding input format
F2LLM-v2-330M (Qwen3 decoder, 896-dim) uses:
- **Documents:** raw text — `title: {name | "none"} | text: {content}` (no instruction prefix)
- **Queries:** `Instruct: <instruction>\nQuery: {q}`
  - Code search: `Instruct: Given a code search query, retrieve the relevant code snippets\nQuery: {q}`
  - Memory/QA: `Instruct: Given a question, retrieve passages that answer the question\nQuery: {q}`

Document format is produced by `Chunk::embedding_text()` in
`crates/spelunk-core/src/indexer/chunker.rs`. Query prefixes are applied by
`handlers.rs` (server-side code search), `embed_query_vec()` in `helpers.rs`
(CLI-side memory search), and `embed_cmd.rs` (plumbing embed --query).

### SQLite + sqlite-vec
No separate vector DB. The sqlite-vec extension adds a `vec0` virtual table
for KNN queries. The extension is registered via `sqlite3_auto_extension`
before any connection is opened (see `crates/spelunk-cli/src/main.rs` and
`crates/spelunk-server/src/main.rs`).

### Incremental indexing
Each file is hashed with blake3. On re-index, unchanged files are skipped.
Changed files: delete old chunks + embeddings, reparse, re-embed.

### Multi-project registry
`~/.config/spelunk/registry.db` tracks all indexed projects and their
dependency links. `spelunk search` automatically queries all linked project DBs
and merges results by distance. Additionally, `spelunk memory search`,
`spelunk memory list`, and `spelunk context` surface `locked`- or
`cross-project`-tagged `decision` and `requirement` entries from linked
projects' memory stores (ADR-003). Each cross-project result is tagged with its
source project so decisions remain attributable. Pass `--local-only` to any of
these commands to query only the primary project's memory. See
`docs/memory.md#cross-project-visibility`.

### Secret scanning
`crates/spelunk-core/src/indexer/secrets.rs` runs before each chunk is stored. Chunks matching
known credential patterns (AWS keys, PEM headers, GitHub PATs, etc.) are
silently dropped and a warning is logged — content is never echoed.

### Prompt structure
The ask prompt uses XML-style delimiters to separate untrusted RAG context
from the user's question, mitigating prompt injection:
```xml
<code_context>
{retrieved chunks}
</code_context>

<question>
{user question}
</question>
```

---

## Supported Languages

Rust, Go, Python, TypeScript, JavaScript, JSX, TSX, Java, C, C++, Ruby,
Swift, Kotlin, JSON, HTML, CSS, HCL, Proto, SQL, Markdown, plain text.

---

## Common Commands

```bash
# Build all crates
cargo build
cargo build --release

# Build specific binaries
cargo build -p spelunk-cli
cargo build -p spelunk-server

# Run the CLI
cargo run -p spelunk-cli -- index ./some/project
cargo run -p spelunk-cli -- search "how does authentication work"
cargo run -p spelunk-cli -- status
cargo run -p spelunk-cli -- status --all
cargo run -p spelunk-cli -- graph <symbol>
cargo run -p spelunk-cli -- chunks src/some/file.rs
cargo run -p spelunk-cli -- languages
cargo run -p spelunk-cli -- sync              # push local memory to server (alias for memory push)

# Run the server
cargo run -p spelunk-server -- --port 7777

# Verbose logging
RUST_LOG=debug cargo run -p spelunk-cli -- index .

# Tests (all crates)
cargo test

# Tests for a specific crate
cargo test -p spelunk-core
cargo test -p spelunk-cli
cargo test -p spelunk-server

# Security audit (requires cargo-audit)
cargo audit
```

---

## Dependency Notes

- Tree-sitter language crate versions must be compatible with the `tree-sitter`
  core. If you bump the core, check all `tree-sitter-*` crates too.
- `sqlite-vec` is loaded at runtime via `sqlite3_auto_extension` (see
  `crates/spelunk-cli/src/main.rs` and `crates/spelunk-server/src/main.rs`).
  The extension binary is bundled by the crate — no system install needed.
- `regex` is used only by `crates/spelunk-core/src/indexer/secrets.rs`. Patterns
  are compiled once via `OnceLock` at the start of `spelunk index`.
- `ignore` respects `.gitignore`, `.ignore`, and global gitignore rules during
  file traversal. Sensitive file patterns (`.env*`, `*.pem`, etc.) are
  excluded unconditionally via `OverrideBuilder`.
- Shared dependency versions are declared in the workspace root `Cargo.toml`
  under `[workspace.dependencies]`. Individual crates inherit them with
  `{ workspace = true }` — bump versions there, not in each crate's `Cargo.toml`.
