# Architecture

This document describes spelunk's system design for contributors and anyone integrating with the codebase.

## Overview

spelunk is a Rust CLI that:

1. **Indexes** source trees using tree-sitter AST parsing
2. **Embeds** each code chunk via an external embedding model
3. **Stores** vectors, chunks, and graph edges in SQLite
4. **Serves** semantic search, graph queries, and memory retrieval via CLI

```
┌─────────────┐     ┌──────────────┐     ┌────────────────┐
│  Source tree │────>│   Indexer    │────>│   SQLite DB    │
│  (.rs, .py,  │     │  tree-sitter │     │  chunks        │
│   .ts, ...)  │     │  + chunker   │     │  embeddings    │
└─────────────┘     └──────┬───────┘     │  graph_edges   │
                           │             │  notes         │
                    ┌──────▼───────┐     └───────┬────────┘
                    │  Embedding   │             │
                    │  Backend     │     ┌───────▼────────┐
                    │  (LM Studio, │     │   Search /     │
                    │   Ollama,    │     │   Graph /      │
                    │   any OAI)   │     │   Memory       │
                    └──────────────┘     └��──────┬────────┘
                                                │
                                        ┌───────▼───────��┐
                                        │   CLI output   │
                                        │  (text / JSON) │
                                        └────────────��───┘
```

## Module structure

```
src/
  main.rs              Entry point: CLI parse, sqlite-vec init, command dispatch
  lib.rs               Library root: re-exports for tests and server binary

  cli/
    mod.rs             Clap structs (Cli, Command, *Args)
    cmd/               One file per subcommand (index.rs, search.rs, etc.)

  config/
    mod.rs             Config struct, loads from ~/.config/spelunk/config.toml
    sync_mode.rs       SyncMode enum: offline / local_first / cloud_first mode selection
    project_id.rs      project-id derivation from git remote / local fallback
    paths.rs           config-dir + project/db discovery
    persist.rs         config.toml / secret-store read-write
    predicates.rs      URL/UUID/env predicates
    tls.rs             custom CA trust-anchor application
    secret_store.rs    OS keychain / file secret-store backend

  backends.rs          Re-exports ActiveEmbedder / ActiveLlm (feature-gated)

  embeddings/
    mod.rs             EmbeddingBackend trait, vec_to_blob/blob_to_vec helpers
    lmstudio.rs        LmStudioEmbedder: POST /v1/embeddings

  llm/
    mod.rs             LlmBackend trait, Message struct
    lmstudio.rs        LmStudioLlm: POST /v1/chat/completions (SSE streaming)

  indexer/
    mod.rs             Re-exports
    chunker.rs         Chunk / ChunkKind structs, embedding_text(), sliding_window
    parser.rs          SourceParser (tree-sitter), detect_language, SUPPORTED_LANGUAGES
    graph.rs           EdgeExtractor: import/call/extends edges via tree-sitter
    secrets.rs         Regex-based credential scanner, drops matching chunks

  storage/
    mod.rs             Re-exports
    db.rs              Database struct: open/migrate, CRUD, KNN search

  search/
    mod.rs             SearchResult struct
    rag.rs             RagPipeline: search + ask methods

  registry.rs          Global project registry (~/.config/spelunk/registry.db)

migrations/            SQL migration files applied in order at DB open
```

## Key design decisions

Architectural decisions are recorded in [docs/adr/](adr/). Key ones:

### Chunking: tree-sitter AST nodes, not line splits

Tree-sitter parses source code into an AST and spelunk extracts named semantic nodes (functions, structs, classes, methods, traits, impls) as individual chunks. This means each chunk is a meaningful unit of code with a name, type, and scope — not an arbitrary 100-line window.

Fallback: a token-aware sliding window for unsupported languages and for oversized semantic nodes that need re-windowing. Each window accumulates whole lines up to `MAX_CHUNK_TOKENS` (512), with ~12.5% token overlap between adjacent windows (the ratio behind the historical 120-line/15-line-overlap split); a single line that alone exceeds the budget becomes its own window so the cap always binds. Re-windowed chunks carry the source node's `name`/`docstring`/`parent_scope` so they still embed with their symbol identity rather than `title: none`. Markdown uses heading-based chunking.

### Storage: SQLite + sqlite-vec, nothing else

All data lives in a single SQLite file per project. The sqlite-vec extension adds a `vec0` virtual table for KNN vector search. No separate vector database, no separate search engine.

This is a deliberate constraint — see [ADR-001](adr/001-scope-boundaries.md). SQLite is zero-configuration, single-file, and sufficient for the scale spelunk targets.

### Incremental indexing via blake3

Each file is hashed with blake3. On re-index, unchanged files are skipped entirely. Changed files get their old chunks and embeddings deleted, then re-parsed and re-embedded.

### Embedding format

Chunks are embedded with **codefuse-ai/F2LLM-v2-330M** (Qwen3 decoder, 896-dim),
served by `spelunk-server` via the candle runtime (Metal/GPU on macOS, CPU on
Linux). Documents use the format:
```
title: {name | "none"} | text: {content}
```

Queries use an instruction prefix: `Instruct: {instruction}\nQuery: {q}`. For
example, code search uses `Instruct: Given a code search query, retrieve the
relevant code snippets\nQuery: {q}`.

See `Chunk::embedding_text()` in `src/indexer/chunker.rs`.

Vectors are L2-normalised and stored as sqlite-vec `INT8[896]` (chunk
embeddings); memory-entry embeddings stay `FLOAT[896]`.

#### Why two vector-storage formats (int8 for chunks, float for memory)

This split is **deliberate**, not an oversight. The two vector tables are sized
for different jobs:

| Table | Type | Rationale |
| --- | --- | --- |
| `embeddings` (chunks) | `INT8[896]` | The code index scales with the corpus — thousands to millions of chunks. int8 scalar quantisation is 4× smaller on disk and, because F2LLM vectors are L2-normalised, lossless enough for ranking. The int8 L2 distance comes back ~127× the f32 distance, so the search path rescales by `embeddings::INT8_SCALE` on read. |
| `note_embeddings` (memory) | `FLOAT[896]` | The memory-note table is tiny (tens to low-thousands of rows per project), so the int8 footprint win is negligible. Keeping full-precision f32 avoids the int8 quantise-on-write + distance-rescale-on-read nuance for a table that never grows large, and keeps the memory insert/search path (`MemoryStore::insert_embedding` / `search`, fed by `embeddings::vec_to_blob`) free of `vec_int8(...)` wrapping and `INT8_SCALE` division. |

Concretely, the two paths never mix:
- **int8 path** — `Database::{insert_embedding,search_similar}` goes through
  `embeddings::vec_to_int8_blob` + `vec_int8(?)` on write and divides the raw
  distance by `embeddings::INT8_SCALE` on read (`storage/db.rs`,
  `storage/search.rs`).
- **float path** — memory notes go through `embeddings::vec_to_blob` (raw
  little-endian f32) into `MemoryStore::insert_embedding`, and `MemoryStore::search`
  matches on the raw f32 query blob with no rescale
  (`storage/memory/notes.rs`, `storage/memory/search.rs`). The server-side memory
  store (`spelunk-server/src/db.rs`) mirrors this float layout.

If memory ever grows to corpus scale, migrating `note_embeddings` to int8 would
be the obvious follow-up — but until then the int8 cost (a second quantised path
to maintain, plus a forced memory re-embed/re-harvest on migration) buys nothing.

The dimension upgrade for pre-0.9 `FLOAT[768]` databases is handled **per store**:
`Database::apply_dim_upgrade_migration` rebuilds the chunk table as
`INT8[896]`, while `MemoryStore::apply_dim_upgrade_migration` (one step in
`MemoryStore::run_migrations`, the same forward-only `PRAGMA user_version`-gated
runner `index.db` uses) rebuilds `note_embeddings` as `FLOAT[896]` (each still
guarded by its own marker table). There is no path that leaves
memory stranded on the stale 768-dim layout. The `note_embeddings` rebuild is
empty rather than converting the old vectors, so semantic recall on pre-upgrade
notes is lost until they are re-embedded with `spelunk memory reindex`; a
one-line notice after the upgrade points the user at that command.

### Backend abstraction

The `EmbeddingBackend` and `LlmBackend` traits (in spelunk-core's `embeddings/` and `llm/`) are the only interface between spelunk and inference. spelunk-core ships **no** concrete implementations. The native F2LLM embedder engine lives in its own `spelunk-embed` library crate (`crates/spelunk-embed/src/embedder_native.rs`, `NativeEmbedder`), which only loads the model from local files already on disk (`load_from_path`) and carries no download dependency. `spelunk-server` depends on that crate, owns the Hugging Face Hub download path that resolves those local files (`crates/spelunk-server/src/embed_hub.rs`), and additionally provides the OpenAI-compatible HTTP clients. The CLI reaches inference only through `ServerInferenceClient` in `crates/spelunk-cli/src/server_client.rs`, with `ServerEmbedAdapter` and `ServerLlmAdapter` as thin trait adapters over it. Embedding and LLM inference are routed by separate rules and can resolve to different servers in a single command, so a caller needing both builds two clients; the LLM rule lives in `crates/spelunk-cli/src/capability/llm_route.rs`.

To add a new backend: implement the trait (in `spelunk-embed` for an embedder, or in spelunk-server for an LLM/HTTP backend) and wire it into the server's endpoint handlers. Nothing in spelunk-core imports a concrete backend.

### Secret scanning

`src/indexer/secrets.rs` runs regex patterns against the full text that will be persisted and embedded for each chunk (docstring + content) before storage, and separately against LLM-generated summaries when they're produced (summaries don't exist yet at chunk-store time). Chunks matching known credential patterns (AWS keys, PEM headers, GitHub PATs, etc.) are silently dropped in full — including their docstring — and a warning naming only the symbol is logged; a secret-bearing summary is stored as an empty string instead.

This scanner is **best-effort defense-in-depth, not a security boundary** — a finite set of regexes cannot catch every credential format. The actual boundary is that code never leaves the local machine unless a team `server_url` is explicitly configured; the scanner only reduces the chance of a credential being embedded/stored (and, on that explicit-server path, transmitted) by accident. This boundary is enforced by `crates/spelunk-cli/tests/egress_containment.rs`, which traps every outbound connection across local-tier CLI flows and fails loudly, naming the destination, on any escape past loopback.

### Multi-project registry

`~/.config/spelunk/registry.db` tracks all indexed projects. `spelunk link` connects projects so that `spelunk search` queries multiple databases and merges results by vector distance.

## Data flow: index

```
files on disk
  → SourceParser (tree-sitter AST → Chunk[])
  → SecretScanner (drop credential chunks)
  → EmbeddingBackend.embed(batch of chunk texts)
  → Database.store(chunks + embeddings)
  → EdgeExtractor (AST → graph_edges)
  → Database.store(edges)
```

## Data flow: search

```
query string
  → EmbeddingBackend.embed(formatted query)
  → Database.search_similar(query_vec, limit)  // sqlite-vec KNN
  → [optional] Database.graph_neighbor_chunks() // 1-hop expansion
  → [optional] query linked project DBs via registry
  → merge + deduplicate by (file_path, start_line, end_line)
  → return Vec<SearchResult>
```

## Adding a new language

1. Add the `tree-sitter-{lang}` crate to `Cargo.toml`
2. Register the language in `src/indexer/parser.rs` (`detect_language` + `SUPPORTED_LANGUAGES`)
3. Add extraction patterns in `src/indexer/graph/edges.rs` for graph edge support
4. Add tests
