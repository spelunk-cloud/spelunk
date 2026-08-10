# ADR-001: Scope Boundaries — What spelunk Is and Isn't

**Status:** Accepted
**Date:** 2026-03-28
**Context:** Competitive landscape analysis against DeepWiki, Augment Code, Cursor, Greptile, Sourcegraph Cody, Aider, Bloop, and Zoekt.

## Decision

spelunk is a **context retrieval engine** for AI agents. The following boundaries define what it will and will not become.

### 1. Context retrieval, not code generation

spelunk retrieves and ranks code context. It does not generate, modify, or refactor code.

The `ask` and `explore` commands use an LLM to synthesise answers *about* code, but the output is explanatory text, never a code patch or file modification. Code generation is the agent's job. spelunk feeds the agent; the agent acts.

**Why:** Coupling retrieval and generation creates a monolith that competes with every AI coding assistant. Staying retrieval-only makes spelunk composable — it works with Claude Code, Aider, Cursor, or any future agent. The moment spelunk writes code, it becomes one more agent instead of infrastructure.

**Boundary test:** If a proposed feature would modify a source file in the indexed project, it belongs in the agent, not in spelunk.

### 2. SQLite is the storage layer

All persistent state — chunks, embeddings, graph edges, memory, registry — lives in SQLite databases. The sqlite-vec extension provides vector KNN search. FTS5 (bundled with SQLite) provides full-text search.

spelunk will not adopt a separate vector database (Qdrant, Milvus, Turbopuffer, etc.) or a separate search engine (Tantivy, Elasticsearch, etc.).

**Why:** SQLite is zero-configuration, single-file, and embedded. It eliminates an entire category of operational complexity (servers, ports, connection strings, version compatibility). sqlite-vec and FTS5 are sufficient for the scale spelunk targets — single developer, local machine, repos up to ~100K files. If a user's codebase outgrows SQLite's capabilities, they likely need Sourcegraph, not spelunk.

**Boundary test:** If a proposed storage change requires the user to run a separate process or manage a separate data directory, reject it.

### 3. Embedding-model agnostic, not embedding-model provider

> **Superseded (2026-07-26).** spelunk now bundles and pins a single embedding
> model (F2LLM-v2-330M@896) as the sole compute path, product-wide; the
> configurable, agnostic OpenAI-compatible embedding backend this section
> describes, including any option to relocate where embeddings are computed,
> no longer exists. Kept as the original record of the reasoning at the time.
> See `docs/third-party-models.md` and `docs/config-reference.md` for current
> behavior.

spelunk calls an embedding API. It does not bundle, host, fine-tune, or distribute embedding models.

The backend trait (`EmbeddingBackend`) accepts any service that speaks the OpenAI-compatible `/v1/embeddings` protocol. LM Studio is the default because it runs locally, but any compatible endpoint works (Ollama, vLLM, OpenAI, etc.).

**Why:** Embedding models improve rapidly. Bundling a model locks spelunk to a quality ceiling and adds a multi-hundred-MB binary. Staying agnostic means users get better retrieval for free by loading a better model, without waiting for a spelunk release.

**Boundary test:** If a proposed feature requires a specific model architecture, weights file, or tokenizer, it belongs in the backend, not in spelunk core.

### 4. Complement agentic search, don't replace grep

The 2026 industry trend is agentic search: LLMs driving iterative grep/file-read loops. This works well for exact symbol lookup and is always fresh. spelunk's value is in what grep *cannot* do:

- **Semantic search** — finding code by concept when you don't know the symbol name
- **Structural graph queries** — "what calls this function" across file boundaries
- **Cross-project search** — querying linked dependency projects
- **Ranked context selection** — returning the most important code within a token budget

spelunk should never try to be a faster grep. When an agent knows the exact string, `rg` is the right tool. spelunk is for when the agent doesn't know what to grep for.

**Why:** Competing with grep on exact match is a losing proposition — it's instant, zero-setup, and universally available. Investing in "spelunk grep" dilutes focus from the semantic and structural capabilities that are genuinely differentiated.

**Boundary test:** If a proposed search feature would be better served by `rg --type rust "pattern"`, it doesn't belong in spelunk.

## Consequences

- Feature proposals are evaluated against these four boundaries before planning.
- The CLAUDE.md agent guide references this ADR for architectural context.
- These boundaries may be revised via a new ADR if the competitive landscape or project goals change materially.
