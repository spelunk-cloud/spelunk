# CLAUDE.md — spelunk

Developer guide for AI agents (and humans) working on this codebase.

---

## Agent workflow — use spelunk on this codebase

This project is indexed with spelunk. Use it — don't just use Read/Grep/Glob.

**At the start of every session:**
```bash
spelunk context                                   # pull prior decisions, handoffs, questions, requirements (compact by default; --budget <N> caps output tokens)
spelunk check                                     # verify index is fresh (only if indexed)
```

**Before reading any file, search first:**
```bash
spelunk graph <symbol>                            # trace callers/callees (works live even without an index)
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

**Built-in (no inference server or cloud dependency):** git-notes memory, full-text search, code graph (AST + call edges), tree-sitter chunking. Full-text search and `spelunk graph <symbol>` run live even in an uninitialized directory; the index-backed paths (`chunks`, `check`, memory, and `graph` on a file path) need `spelunk init` first.

**Semantic search via spelunk-server:** from v0.9.0 the default UX runs a local `spelunk-server` (auto-bound on `127.0.0.1`). The server bundles a native embedder (codefuse-ai/F2LLM-v2-330M, 896-dim, candle runtime, Metal/GPU on macOS) — no external embedding endpoint required. Semantic search, `spelunk explore`, `spelunk memory harvest`, and LLM summaries all route through the server's inference endpoints; the CLI talks to it via `server_client.rs`. Manage the daemon with `spelunk server start|stop|status|logs`. This **auto-discovered loopback server is an inference backend only** — it embeds queries and runs LLM calls, but it is **never** a memory store. A project's memory always lives in its local `memory.db`; the loopback server holds no authoritative memory.

**Optional: team memory server** (`server_url` *explicitly* set in config, pointing at a shared instance): share memory (decisions, requirements) across a team. Setting an explicit `server_url` is the **only** way memory moves off the local `memory.db` — it relocates the store of record to the shared server. Each developer's code stays local. (Note the distinction: an auto-discovered loopback server provides inference and never owns memory; an explicit team `server_url` does own memory. They must not be conflated.) When a team server routes projects by an internal UUID, a human `project_id` slug is auto-resolved to that UUID on first use and cached in `.spelunk/cloud-project-id.lock` (see ADR-005); a raw UUID `project_id` is used directly.

You search with spelunk, then reason over the results yourself.

---

## Workspace Structure

This is a Cargo workspace with four crates:

```
Cargo.toml                    — workspace root; [workspace.dependencies] for shared versions

crates/
  spelunk-core/               — library: storage, indexer, embeddings, LLM, search, config, registry
  spelunk-cli/                — `spelunk` binary; depends on spelunk-core
  spelunk-embed/              — library: native F2LLM-v2-330M embedder (candle); depends on spelunk-core
  spelunk-server/             — `spelunk-server` binary + lib; depends on spelunk-core + spelunk-embed
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
embed_hub.rs       — Hugging Face Hub download path for the bundled native embedder (gated by
                     `embed-native`); fetches the pre-quantized GGUF/tokenizer/config to disk, then
                     calls spelunk-embed's `NativeEmbedder::load_from_path`. The only place in the
                     workspace that depends on `hf-hub`.

migrations/  (crates/spelunk-server/migrations/)
  server_001.sql — projects + server memory schema
  server_002.sql — server memory FTS
```

The native embedder engine lives in the `spelunk-embed` crate (below). The
server's `embed-native` feature enables the optional `spelunk-embed` dep (and
the server's own hf-hub download path); `metal` forwards to `spelunk-embed`'s
`metal` feature. `spelunk-embed` depends on candle unconditionally — the crate
is only worth depending on for its native embedder, so there is no feature to
gate candle out. Its only remaining feature is `metal` (macOS GPU accel).

### spelunk-embed (`crates/spelunk-embed/src/`)

```
lib.rs             — crate root; re-exports NativeEmbedder + DIM (candle is unconditional)
embedder_native.rs — native embedder (F2LLM-v2-330M via candle, 896-dim, Metal/GPU on macOS).
                     NativeEmbedder::load_from_path(gguf, tokenizer, config) loads local files
                     already on disk with zero network access — the crate's only load entry
                     point, and it carries no download/fetch dependency. Implements
                     spelunk-core's EmbeddingBackend. spelunk-server's embed_hub module (above)
                     resolves those local files via the Hugging Face Hub before calling it.
```

---

## Inference Backend

All AI inference goes through **spelunk-server**. The CLI calls the server via
`ServerLlmClient` and `ServerEmbedClient` in `crates/spelunk-cli/src/server_client.rs`
— these are the only places in spelunk-cli that issue AI inference requests.

`spelunk-core` defines the `EmbeddingBackend` and `LlmBackend` traits
(`embeddings/mod.rs`, `llm/mod.rs`) but ships **no concrete implementations**.
The native embedding *engine* lives in the `spelunk-embed` crate
(`NativeEmbedder`, local-path load only); spelunk-server's `embed_hub` module
owns the Hugging Face Hub download path that resolves the (pre-quantized)
model artifacts before handing them to it. The LLM backend and the external
HTTP embedder shim live in spelunk-server (`main.rs`).

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

Chunk embeddings are stored as `INT8[896]` (F2LLM vectors are
L2-normalised, so int8 is lossless enough for ranking and ~4x smaller on disk);
the int8 L2 distance is rescaled back to the f32 scale by `INT8_SCALE` on read
(`storage/search.rs`). Memory-entry embeddings stay
`FLOAT[896]`. On first open, `db.rs` detects pre-0.9 `FLOAT[768]` `vec0` tables
and drops and recreates them as `INT8[896]` (re-index required).

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
`crates/spelunk-core/src/indexer/secrets.rs` runs before each chunk is stored, scanning the full
text that will be persisted and embedded (docstring + content; LLM summaries are scanned
separately when generated, since they don't exist yet at store time). Chunks matching known
credential patterns (AWS keys, PEM headers, GitHub PATs, etc.) are silently dropped — including
their docstring, so nothing lands in stored metadata either — and a warning naming only the
symbol is logged; a secret-bearing summary is replaced with an empty string instead of being
stored. **This is best-effort defense-in-depth, not a security boundary**: a finite regex list
cannot catch every credential format. The real boundary is that code never leaves the local
machine unless a team `server_url` is explicitly configured (see above); the scanner only reduces
the chance of a credential being embedded/stored (and, on that explicit-server path, transmitted)
by accident.

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

Rust, Go, Python, TypeScript, JavaScript, JSX, TSX, Java, C, C++, PHP, Ruby,
C#, Swift, Kotlin, JSON, HTML, CSS, HCL, Proto, SQL, Markdown, plain text.

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

# Verify a change before pushing: the Check & Lint + Test legs of CI.
# CI calls these same make targets, so green here and green CI agree.
make check

# Individual gates (see `make help` for the full list)
make lint      # fmt --all --check, clippy, check, build (all with rich-formats)
make test      # nextest + doctests, for default AND --no-default-features
make fmt       # reformat in place

# Tests for a specific crate (nextest is what CI runs; plain `cargo test` is not)
cargo nextest run -p spelunk-core
cargo nextest run -p spelunk-cli
cargo nextest run -p spelunk-server

# Security audit (requires cargo-audit)
make audit
```

**Do not hand-assemble the verification command: run `make check`.** Every ad-hoc
variant of it has been wrong in at least one way, always in the direction of a false
green:

- CI runs `cargo nextest run`, **not** `cargo test`. Consequence: `#[serial]` is a
  no-op under nextest (every test gets its own process), so a test relying on it to
  serialise shared *external* state (a file, a port, a git ref) is unguarded in CI.
- CI's clippy is `--all-targets --features rich-formats -- -D warnings`. Dropping the
  feature lints **weaker** than CI: a warning only reachable under it passes locally
  and fails CI.
- nextest does not run doctests. CI runs `cargo test --doc` separately, per feature
  config.
- `SPELUNK_SECRET_STORE=file` must be set or macOS pops Keychain dialogs mid-run. The
  make targets export it.

Never pipe a gate into `tail`/`head` to shorten output. zsh has no `PIPESTATUS`: that is
a bash name, and zsh's array is the lowercase, 1-indexed `pipestatus`. So `${PIPESTATUS[0]}`
and `${PIPESTATUS[1]}` are both empty, and a bare `$?` after the pipeline is `tail`'s status,
not the gate's. Either way the failure is hidden. Run the target and read its exit status.

`make check` does **not** cover every CI job. The Windows test leg, Docker image build,
`cargo audit`/`cargo deny`, the OpenAPI snapshot check and the weekly fuzz run are
listed with their opt-in targets in
[docs/building.md](docs/building.md#what-make-check-does-not-cover). To get the real CI
signal on a branch without opening a PR, which is the only way to reach the Windows
leg, run `gh workflow run ci.yml --ref "$(git branch --show-current)"`.

---

## Dependency Notes

- Tree-sitter grammars come from **`ast-grep-language`** (a single crate,
  ABI-aligned to the `tree-sitter` core). Bump that one crate instead of many
  `tree-sitter-*` deps. `proto` and `sql` are the only exceptions —
  ast-grep-language doesn't ship them, so they stay on the standalone
  `tree-sitter-proto` / `tree-sitter-sequel` crates. If you bump the
  `tree-sitter` core, check that `ast-grep-language` (and the two standalone
  grammars) still resolve to the same runtime line. `ast-grep-core` provides the
  in-process structural-search fallback (`crates/spelunk-core/src/search/live.rs`).
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
