# spelunk

[![CI](https://github.com/spelunk-cloud/spelunk/actions/workflows/ci.yml/badge.svg)](https://github.com/spelunk-cloud/spelunk/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust edition 2024](https://img.shields.io/badge/rust-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/)

**Code intelligence for AI agents — zero infrastructure required.** Persistent memory, code graph, and search that work straight from the CLI.

```bash
spelunk graph validate_token                       # trace callers, callees, imports
spelunk search "error handling" --mode text        # full-text search, no server needed
spelunk memory add --kind decision --title "Chose sqlite-vec" --body "..."  # persistent across sessions
```

Semantic search works out of the box: `spelunk` autostarts a local `spelunk-server` that bundles a native embedder — no external inference server to run. Point everyone at a shared `spelunk-server` to share memory across a team.

## Quick start

**1. Install**

```bash
curl -fsSL https://raw.githubusercontent.com/spelunk-cloud/spelunk/refs/heads/main/install.sh | sh
```

> Also available via Homebrew (`brew install spelunk-cloud/spelunk/spelunk`), a
> Debian `.deb`, or a tarball from the
> [releases page](https://github.com/spelunk-cloud/spelunk/releases). See
> [Getting Started](docs/getting-started.md) for all install paths.

**2. Use it immediately — no setup required**

From inside any git repository:

```bash
spelunk graph validate_token                       # trace callers and callees
spelunk search "error handling" --mode text        # full-text search
spelunk memory add --kind decision \
  --title "Chose token bucket for rate limiting" \
  --body "Simpler than sliding window; sufficient for <1k RPS"
spelunk memory list --kind decision
spelunk context                                    # agent session entry point
```

**3. Add semantic search**

`spelunk init` indexes your project and starts the bundled server, so semantic
search works with no extra setup:

```bash
spelunk init                                       # index + autostart server in one step
spelunk search "error handling in the HTTP layer"  # semantic search
spelunk search "database migrations" --graph       # with callers/callees
```

## Why spelunk?

AI coding agents lose context between sessions and can't trace how code connects across files. spelunk solves both with zero infrastructure.

- **Persistent memory** — store decisions, requirements, and context in git notes. Retrieve them next session, or share them via a server with your team.
- **Code graph** — trace callers, callees, and imports across file boundaries without reading every file.
- **Works without any server** — memory, code graph, and full-text/structural (ast-grep) search work with just the binary. No API keys, no configuration.
- **Semantic search built in** — a local `spelunk-server` is autostarted on demand with a bundled native embedder (codefuse-ai/F2LLM-v2-330M, 896-dim, GPU-accelerated on macOS); no external inference server required. You can still point spelunk at your own OpenAI-compatible endpoint (LM Studio, Ollama, vLLM) if you prefer.
- **100% local** — your code never leaves your machine. The server is self-hosted (local by default).
- **Agent-native** — JSON output (`AGENT=true`), git hooks, and a structured memory system built for the agent workflow loop.

### When to use spelunk vs grep

| You want to... | Use |
|---|---|
| Find an exact function name | `rg "fn validate_token"` |
| Find code related to a concept | `spelunk search "request authentication"` |
| See what calls a function | `spelunk graph validate_token` |
| Remember why a decision was made | `spelunk memory search "why sqlite-vec"` |
| Store a design decision for future sessions | `spelunk memory add --kind decision ...` |
| Share context across a team | `spelunk-server` + `server_url` |

## Core features

### Project memory

Store decisions, requirements, and context that persist across sessions — in git notes, no server needed:

```bash
spelunk memory add --kind decision --title "Chose sqlite-vec over pgvector" \
  --body "Must run without a Postgres server. Revisit if we need filtering + ANN."
spelunk memory list --kind decision --limit 10
spelunk memory search "why did we choose this database"
spelunk memory harvest   # auto-extract decisions from recent commits (server with LLM backend)
spelunk sync             # two-way sync of local memory with the configured server (push + pull)
```

Memory is stored in local SQLite and written through to git notes by default
(`store_in_git_notes`), so it travels with the repo. Set `server_url` to share
across a team.

### Code graph

```bash
spelunk graph RagPipeline                        # all edges for a symbol
spelunk graph src/storage/db.rs --kind imports   # imports in a file
```

spelunk extracts import, call, extends, and implements edges from the AST. No index or server needed.

### Search

```bash
spelunk search "handleRequest" --mode text       # full-text, no server needed
spelunk search "how are errors propagated"       # semantic (requires server + index)
spelunk search "auth middleware" --graph         # expand with 1-hop callers/callees
spelunk search "request handling" --budget 4000  # fit results within a token budget
```

### Agentic exploration

```bash
spelunk explore "how does incremental indexing work?"   # LLM iterates search + graph to answer
spelunk explore "what guards the context window?" --verbose
```

`explore` requires a server with an LLM backend configured.

### Multi-project search

```bash
spelunk link ../shared-utils
spelunk search "connection pooling"   # searches both projects, merges by relevance
```

### Agent integration

Set `AGENT=true` for JSON output on every command:

```bash
AGENT=true spelunk memory list --kind decision
AGENT=true spelunk graph validate_token
AGENT=true spelunk search "auth flow" | jq '.[0].file_path'
```

Install git hooks to auto-harvest memory on every commit:

```bash
spelunk hooks install
```

spelunk ships with a [Claude Code skill](SKILL.md) and [agent guide](docs/agent-guide.md) for integration with AI coding agents.

## Supported languages

Tree-sitter AST-aware chunking for: **Rust**, **Go**, **Python**, **TypeScript**, **JavaScript**, **JSX**, **TSX**, **Java**, **C**, **C++**, **PHP**, **Ruby**, **C#**, **Swift**, **Kotlin**, **JSON**, **HTML**, **CSS**, **HCL**, **Proto**, **SQL**, **Markdown**.

All other file types are indexed as plain text with a sliding-window chunker.

## Documentation

Start at the **[documentation home](docs/README.md)**, which walks the path from
the first five minutes to running a shared memory server for a team.

- [Getting Started](docs/getting-started.md): install, index your first project, run your first retrieval
- [Memory](docs/memory.md): decisions, context, and requirements across sessions
- [Agent Guide](docs/agent-guide.md): wiring spelunk into AI coding agents
- [Commands](docs/commands.md): full reference for every subcommand
- [Architecture](docs/architecture.md): system design for contributors
- [Examples](docs/examples/): real-world workflows

## Repository structure

This is a Cargo workspace with three crates:

| Crate | Path | Purpose |
|---|---|---|
| `spelunk-core` | `crates/spelunk-core` | Library — storage, indexer, embeddings, LLM, search, config, registry |
| `spelunk-cli` | `crates/spelunk-cli` | `spelunk` binary — CLI commands; depends on `spelunk-core` |
| `spelunk-server` | `crates/spelunk-server` | `spelunk-server` binary + lib — shared memory server; depends on `spelunk-core` |

```bash
cargo build -p spelunk-cli    # build the CLI
cargo build -p spelunk-server # build the server
make check                    # run the lint + test gates CI runs
```

## Contributing

Contributions welcome. See [Building from source](docs/building.md) for setup instructions.

Before pushing, run:

```bash
make check
```

This runs the **Check & Lint** and **Test** legs of CI. CI calls the same make targets,
so a green `make check` and a green CI agree. It does not cover every job: the Windows
tests, Docker image build, `cargo audit` / `cargo deny`, the OpenAPI snapshot check and
the nightly fuzz run are listed, with their opt-in targets, in
[Building from source](docs/building.md#what-make-check-does-not-cover).

Note: In zsh, never pipe `make check` into `tail` or `head`: the pipeline masks the exit
status and reports false green. Run it directly. Also, CI runs `cargo nextest run` (not
`cargo test`), so `#[serial]` does not actually serialize tests in CI.

To get the real CI signal on a branch without opening a pull request (the only way to
reach the Windows test leg):

```bash
gh workflow run ci.yml --ref "$(git branch --show-current)"
```

## License

[MIT](LICENSE)

spelunk-server bundles a third-party embedding model (Apache-2.0). See
[Third-party models](docs/third-party-models.md) for attribution.
