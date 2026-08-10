# Agent Guide

`spelunk` is designed to work as infrastructure for AI coding agents, not just as a human developer tool. This guide covers the patterns that make agents most effective when paired with `spelunk`.

**The key mental model**: spelunk retrieves context; you reason over it. Use `spelunk graph` and `spelunk search` to find the right code, read the results, then synthesise the answer yourself. spelunk is a persistent memory store and code navigation tool, not an oracle.

**What's built-in:** memory (local SQLite `memory.db`, optionally mirrored to git-notes), code graph, full-text and ast-grep search, and extracted conventions work with just the CLI binary — no server needed. A project's memory always lives in its local `memory.db`; that is the canonical store of record for every memory command.

**What's server-backed:** semantic/hybrid search (`spelunk search --mode auto|semantic|hybrid`), `spelunk explore`, and `spelunk memory harvest` use `spelunk-server` for **inference** (embeddings + LLM). From v0.8.0 the server is autostarted locally on demand and bundles a native embedder (codefuse-ai/F2LLM-v2-330M, 896-dim, GPU-accelerated on macOS via candle) — there is no external embedding server to run by default. The auto-discovered loopback server is **inference-only**: it never stores memory. For `memory search` the CLI sends only the query to the loopback embedder and runs the vector search locally against `memory.db` — note text never leaves the local store. If you force offline mode (`SPELUNK_NO_SERVER=1`), these commands fall back to text/ast-grep search or error clearly, and all memory commands operate on `memory.db`.

**Where does memory live?** Always `memory.db` for the active project — **unless** you have *explicitly* configured a team `server_url`, which relocates the store of record to that shared server (the team-memory tier). An auto-discovered loopback server does **not** change where memory lives.

## The core loop

A productive agentic session with `spelunk` looks like this:

1. **Orient** — read memory and check index health (`spelunk context`, `spelunk check`)
2. **Search** — find the relevant code before reading or editing it
3. **Execute** — make code changes, delegating sub-tasks as needed
4. **Verify** — re-check the call graph and re-index after changes
5. **Codify** — store decisions, handoffs, and context in memory

This loop compounds: each session leaves better context for the next, whether that's the same agent resuming or a different one picking up.

## Machine-readable output

Set `AGENT=true` and every `spelunk` command returns JSON:

```bash
export AGENT=true

spelunk search "error handling"          # → JSON array of results
spelunk status                           # → { files, chunks, embeddings, ... }
spelunk memory list                      # → JSON array of notes
spelunk memory search "auth decisions"   # → JSON array of notes with distance scores
```

You can also use `--format json` on individual commands.

## Managing the local server daemon

If your config does not have a `server_url`, `spelunk` auto-discovers a local
`spelunk-server` running on loopback by reading
`~/.local/state/spelunk/server.port`.  You can start, stop, and inspect that
daemon with the `spelunk server` subcommand. This auto-discovered daemon is an **inference backend only** — it serves embeddings and LLM calls. It is **not** a memory store: your project's memory stays in `memory.db` regardless of whether this server is running. (Memory moves to a server only when you *explicitly* set `server_url` to a team instance in your config.)

```bash
# Start spelunk-server on port 7777 (idempotent — no-op if already running)
spelunk server start

# Check whether the daemon is running and get its PID/port/version
spelunk server status

# Tail the last 50 lines of the server log
spelunk server logs

# Stop the daemon gracefully (SIGTERM; waits up to 10 s)
spelunk server stop
```

**State directory:** all runtime files (`server.pid`, `server.port`,
`server.log`) live under `~/.local/state/spelunk/`.

**Idempotency:** `spelunk server start` is safe to call at the beginning of
every session.  If the daemon is already running and healthy it exits 0
immediately.  If the PID is stale (process dead), it starts a fresh instance.

**When to use `status` vs probing `/v1/health` directly:** use
`spelunk server status` for human-readable output during debugging.  For
programmatic checks inside an agent loop, `spelunk check` already probes the
server as part of its index-freshness check — you rarely need to poll
`/v1/health` directly.

**Port walk:** `start` tries ports 7777–7787 in order.  If all are taken it
exits with a clear error.  Use `--port <n>` to override the starting port.

## Starting a session

At the start of a session, orient yourself:

```bash
# Agent session entry point — pulls context from previous sessions
spelunk context

# If you've indexed: verify the index is up to date
spelunk check
```

`spelunk context` is designed as the single agent entry point. It retrieves the four most agent-relevant memory sections (handoffs, open questions, decisions, requirements) sorted newest-first, giving the agent a full picture of prior work.

Flags:
- `--format json` — machine-readable output
- `--kind decision` — narrow to one section
- `--path src/auth` — filter by file path tag
- `--limit N` – entries per section (defaults: handoff=3, question=10, decision=10, requirement=10); mutually exclusive with `--budget`
- `--budget N` (alias `--max-tokens`) – cap total output at N tokens; mutually exclusive with `--limit`. Under a tight budget, durable decisions and requirements are kept ahead of open questions.
- `--no-conventions` — skip the extracted-conventions section

`spelunk context` also surfaces a **conventions** section: coding conventions
inferred by a heuristic AST pass over the index (no LLM). It needs an index but
no server.

## Searching before writing

Before modifying any file, search for related code:

```bash
# Trace the call graph around a symbol (no server needed)
AGENT=true spelunk graph validate_token

# Full-text search (no server needed)
AGENT=true spelunk search "authentication middleware" --mode text

# Get the raw chunks for a specific file (requires index)
AGENT=true spelunk chunks src/auth/middleware.rs

# Semantic search with call-graph expansion (requires server + index)
AGENT=true spelunk search "authentication middleware" --graph
```

The `--graph` flag adds 1-hop callers and callees to the result set — the right context for understanding blast radius before a change.

## Retrieving targeted context

Use `spelunk graph` and `spelunk search` to find relevant code, then read and reason over the results yourself:

```bash
# Trace call chains (no server needed)
AGENT=true spelunk graph handle_request
AGENT=true spelunk search "request lifecycle middleware" --mode text --limit 20 --format json

# Semantic search (requires embedding server + index)
AGENT=true spelunk search "embedding format storage" --graph --format json
```

For open-ended questions that require synthesis across multiple code paths, use `spelunk explore` (requires embedding server and optionally a chat model). It runs an iterative search-and-reason loop:

```bash
AGENT=true spelunk explore "how does incremental indexing decide which files to skip?"
AGENT=true spelunk explore "where is the embedding model loaded?" --max-steps 3
```

`explore` is slower than `search` (multiple LLM calls) — use it only when `search` alone isn't enough.

## After making changes

```bash
# Confirm call sites still match using the code graph (symbol queries work live even without an index)
spelunk graph validate_token --kind calls

# If the project is indexed: re-index changed files (incremental, blake3-gated)
spelunk index .
```

To exclude files or directories from indexing, add a `.spelunkignore` file (same syntax as `.gitignore`) at any directory. It takes higher precedence than `.gitignore`. Indexing also applies a built-in filter that skips generated, vendored, minified, and machine-data files (lockfiles, `node_modules/`, `*.min.js`, protobuf codegen, and files that self-declare `@generated`); tune it with the `[index]` table in config. See [File filtering](commands.md#file-filtering).

**Note:** Indexing is optional and only needed if you use semantic search. If you only use `spelunk graph` and full-text search, there's nothing to rebuild after changes.

## Storing decisions

Every non-obvious choice should be stored:

```bash
spelunk memory add \
  --title "Chose sqlite-vec over hnswlib for vector search" \
  --body "No C++ dependency, single file, good enough performance for <1M vectors. Revisit if we need ANN at scale." \
  --kind decision \
  --tags storage,embeddings
```

Doing this consistently means future agents (and future you) can retrieve the rationale:

```bash
spelunk memory search "why did we choose sqlite-vec"
```

**git-notes write-through:** with `store_in_git_notes` enabled (the default),
`spelunk memory add` also appends the entry to `refs/notes/spelunk` on `HEAD`,
so decisions travel with the code through clone/fetch. It is a graceful no-op
outside a git repository. Set `store_in_git_notes = false` to disable.

To inspect that write-through by hand with stock git, name the `spelunk` ref.
Plain `git notes show HEAD` reads git's default `commits` ref and reports "no
note found", a false negative that makes it look like nothing was written:

```bash
git notes --ref=spelunk show HEAD    # notes on the current commit
git notes --ref=spelunk list         # every commit carrying spelunk notes
# equivalently
GIT_NOTES_REF=refs/notes/spelunk git notes show HEAD
```

## Automatic capture (no authoring tax)

Recording decisions by hand is the part that never happens under deadline. The
payoff of wiring an agent to spelunk is that the why-layer fills itself as a
by-product of normal work, with no separate step to sit down and write docs.

Install the git hook once:

```bash
spelunk hooks install
```

The post-commit hook then runs `spelunk memory harvest` after every commit,
using the LLM to extract decisions, requirements, and context from the commit
messages your agent already writes. Teammates without spelunk installed are
unaffected (the hook is a no-op when `spelunk` is not on `PATH`).

You can also harvest on demand, over a range of history or straight from an
agent's own session log:

```bash
spelunk memory harvest --git-range HEAD~20..HEAD    # from commit messages (default source)
spelunk memory harvest --source claude-code --confirm   # from Claude Code session history (reads ~/.claude/history.jsonl)
```

Harvesting needs a server with an LLM backend (the local one autostarts). The
result: every later `spelunk context` / `spelunk search` starts returning the
reasoning behind the code, not just the code, without anyone stopping to author
it. Harvest is additive and idempotent, so re-running it does not duplicate
entries.

## Storing questions for async resolution

When you hit a decision point mid-task:

```bash
spelunk memory add \
  --title "Should verify re-embed from disk or from stored chunk content?" \
  --kind question \
  --tags verify,indexer
```

Pick it up later:

```bash
AGENT=true spelunk memory list --kind question
```

When resolved:

```bash
spelunk memory add \
  --title "verify re-embeds from stored chunk content" \
  --body "Avoids file I/O and keeps behaviour consistent with what was originally indexed. Disk content may have changed since last index." \
  --kind answer \
  --tags verify,indexer
```

## Signalling intent

Use the `intent` kind to broadcast to teammates (human or agent) that you are actively working on a given area. Active intents are surfaced by `spelunk check` along with file overlap warnings, so collaborators see ongoing work before starting overlapping changes.

```bash
spelunk memory add \
  --title "Refactoring auth middleware to support OAuth2" \
  --kind intent \
  --tags auth,middleware \
  --files src/auth/middleware.rs
```

When the work is done, archive the intent:

```bash
spelunk memory archive <id>
```

## Handing off between sessions

At the end of a session, write a handoff note:

```bash
spelunk memory add \
  --title "Handoff: rate limiting plan 60% done" \
  --body "Implemented token bucket in src/ratelimit/bucket.rs. Next: wire middleware, add tests, update docs. Open question: should limits be per-IP or per-API-key?" \
  --kind handoff
```

At the start of the next session, read it:

```bash
spelunk context
```

## Multi-agent coordination

When using a shared memory server (`server_url` in config), agents can coordinate without stepping on each other's toes:

```bash
# Poll for new entries since a given timestamp
spelunk memory since <epoch>

# Stream entries as they arrive (requires an explicit `server_url`; an
# auto-discovered loopback server does not satisfy this)
spelunk memory watch
```

Conflict detection: If you write an entry semantically similar to an existing one (cosine ≥ 0.92), the server returns HTTP 409 (advisory). The entry is stored with a `contradicts` edge linking to the conflicting entry. Check `spelunk memory show <id>` to review related entries before proceeding.

## Reconciling memory from a server database

If you have access to a `spelunk-server` SQLite database (e.g. a team server snapshot or a local server DB at `~/.local/state/spelunk/server.db`), you can import its memory entries into your project's local database without running the server:

```bash
# Preview what would be imported (no writes)
spelunk memory reconcile --source-db ~/.local/state/spelunk/server.db --dry-run

# Import memory from the server DB for the current project
spelunk memory reconcile --source-db ~/.local/state/spelunk/server.db

# Import across all projects in the server DB
spelunk memory reconcile --source-db ~/.local/state/spelunk/server.db --all-projects

# Machine-readable output (one JSON object per imported entry)
spelunk memory reconcile --source-db ~/.local/state/spelunk/server.db --format json
```

Reconcile is additive and idempotent — entries already present in the local DB are skipped (matched by content hash). Useful for seeding a fresh checkout with team decisions, or for offline work after a period connected to a shared server.

## Cross-project search

If your project depends on shared libraries you've indexed separately:

```bash
spelunk link ../shared-utils
spelunk link ../api-contracts
```

Now `spelunk search` queries all three indexes and merges results by distance.

## CI integration

```bash
# Fail the build if the index is stale
spelunk check || { echo "Run spelunk index"; exit 1; }

# Print a GitHub Actions workflow hook
spelunk hooks install --ci
```

## Plumbing Commands

Plumbing commands emit JSONL to stdout and follow a strict exit-code convention, making them safe to use in scripts and pipelines. See [Plumbing and Porcelain](plumbing-and-porcelain.md) for a full explanation of the design philosophy.

Exit codes across all plumbing commands:
- **0** — success, results emitted
- **1** — no results (empty set, not an error)
- **2** — hard error (bad flags, missing DB, I/O failure) — diagnostics on stderr

Commands marked **(requires server)** need a running `spelunk-server` with its embedder ready.

### cat-chunks *(requires index)*

```
spelunk plumbing cat-chunks <file>
```

Emit all indexed chunks for a given file as JSONL.

| Flag | Description |
|------|-------------|
| `<file>` | Project-relative path of the file to retrieve chunks for (required). |

Exit codes: `0` = chunks found, `1` = file has no indexed chunks, `2` = error.

Example:

```bash
spelunk plumbing cat-chunks src/indexer/chunker.rs \
  | jq '{name: .name, lines: "\(.start_line)-\(.end_line)"}'
```

```json
{"name":"sliding_window","lines":"45-78"}
{"name":"Chunk","lines":"12-32"}
```

---

### ls-files *(requires index)*

```
spelunk plumbing ls-files [--prefix <prefix>] [--stale] [--root <dir>]
```

List every indexed file as JSONL. With `--stale`, only files whose on-disk blake3 hash differs from the stored hash are emitted.

| Flag | Description |
|------|-------------|
| `--prefix <prefix>` | Restrict output to files whose path starts with this string. |
| `--stale` | Only emit files that are out of date (on-disk hash ≠ stored hash). |
| `--root <dir>` | Project root for resolving relative paths (defaults to CWD). |

Exit codes: `0` = at least one file emitted, `1` = no files matched, `2` = error.

Example:

```bash
spelunk plumbing ls-files --stale --root .
```

```json
{"path":"src/indexer/chunker.rs","language":"rust","chunk_count":12,"indexed_at":1713528000,"stale":true}
```

---

### parse-file

```
spelunk plumbing parse-file <file>
```

Parse a file with tree-sitter and emit chunks as JSONL without writing anything to the index. Useful for previewing how spelunk will chunk a file.

| Flag | Description |
|------|-------------|
| `<file>` | Path to the file to parse (required). |

Exit codes: `0` = chunks emitted, `1` = unsupported file type or empty parse result, `2` = read error.

Example:

```bash
spelunk plumbing parse-file src/config.rs | jq '{kind, name, start_line}'
```

```json
{"kind":"struct","name":"Config","start_line":8}
{"kind":"impl","name":"Config","start_line":42}
```

---

### hash-file

```
spelunk plumbing hash-file <file>
```

Compute the blake3 hash of a file and check whether it matches the hash stored in the index, emitting a single JSON object.

| Flag | Description |
|------|-------------|
| `<file>` | Path to the file to hash (required). |

Exit codes: `0` = always (unless read error), `2` = file not readable.

Example:

```bash
spelunk plumbing hash-file src/config.rs
```

```json
{"path":"src/config.rs","hash":"a3f1...","indexed_hash":"a3f1...","is_current":true}
```

---

### knn *(requires server + index)*

```
spelunk plumbing knn [--limit N] [--min-score F] [--lang <lang>]
```

Read a JSON embedding object from stdin (as produced by `spelunk plumbing embed`) and return the *N* nearest indexed chunks by cosine similarity.

| Flag | Description |
|------|-------------|
| `--limit N` | Maximum number of results (default: `10`). |
| `--min-score F` | Drop results with cosine similarity below this threshold (0.0–1.0, default: `0.0`). |
| `--lang <lang>` | Restrict results to chunks from files of this language (e.g. `rust`, `python`). |

Exit codes: `0` = results found, `1` = no results pass the filters, `2` = error.

Compose with `embed` for a full semantic search pipeline:

```bash
echo "authentication" | spelunk plumbing embed --query | spelunk plumbing knn --limit 5
```

Example output:

```json
{"chunk_id":42,"file_path":"src/auth/middleware.rs","language":"rust","node_type":"function","name":"validate_token","start_line":18,"end_line":54,"content":"...","distance":0.12,"score":0.88}
```

---

### embed *(requires server)*

```
spelunk plumbing embed [--query]
```

Read lines from stdin and emit one JSONL embedding vector per line. Each output object contains the model name, vector dimensionality, and the float vector.

| Flag | Description |
|------|-------------|
| `--query` | Apply the F2LLM query instruction prefix (`Instruct: …\nQuery: …`). Use this flag when the output will be piped into `knn`. Omit it when embedding document text for storage. |

Exit codes: `0` = at least one vector emitted, `2` = stdin is a terminal (not a pipe) or embedding backend unreachable.

Compose with `knn`:

```bash
echo "authentication" | spelunk plumbing embed --query | spelunk plumbing knn --limit 5
```

Example output:

```json
{"model":"f2llm-v2-330m","dimensions":896,"vector":[0.021,-0.043,...]}
```

(The model name is the pinned model id, and the dimensionality reflects the
bundled native embedder: codefuse-ai/F2LLM-v2-330M at 896 dimensions.
Neither is configurable.)

---

### graph-edges

```
spelunk plumbing graph-edges --file <file> | --symbol <symbol>
```

Emit code graph edges (imports, calls, extends/implements) for a file or symbol. At least one of `--file` or `--symbol` is required. When both are provided, results are merged and deduplicated.

| Flag | Description |
|------|-------------|
| `--file <file>` | Project-relative path; emit all edges originating from this file. |
| `--symbol <symbol>` | Symbol name; emit edges where this name appears as source or target. |

Exit codes: `0` = edges found, `1` = no edges matched, `2` = neither flag supplied or DB error.

Example:

```bash
spelunk plumbing graph-edges --symbol validate_token
```

```json
{"source_file":"src/auth/middleware.rs","source_name":"handle_request","target_name":"validate_token","kind":"calls","line":28}
```

---

### read-memory

```
spelunk plumbing read-memory [--kind <kind>] [--id <n>] [--limit N]
```

Emit memory entries as JSONL. Use `--kind` to filter by entry type or `--id` to fetch a single entry.

| Flag | Description |
|------|-------------|
| `--kind <kind>` | Filter by memory kind: `decision`, `question`, `note`, `answer`, `requirement`, `handoff`, `antipattern`. |
| `--id <n>` | Fetch a single entry by its integer id. Exits `1` if not found. |
| `--limit N` | Maximum number of entries (default: `50`). |

Exit codes: `0` = entries found, `1` = no entries matched, `2` = error.

Example:

```bash
spelunk plumbing read-memory --kind decision --limit 5 | jq '{id, title}'
```

```json
{"id":17,"title":"Chose sqlite-vec over hnswlib for vector search"}
{"id":22,"title":"Incremental index skips unchanged files via blake3 hash"}
```

---

## Summary: agent workflow at a glance

```bash
# Session start — all work out of the box
spelunk context                                              # pull all prior context
spelunk context --budget 4000                               # cap total output at ~4000 tokens
AGENT=true spelunk context --format json                    # machine-readable

# Before writing code — retrieve context, reason yourself
AGENT=true spelunk graph <symbol>                             # call graph
AGENT=true spelunk search "<topic>" --mode text              # full-text search
AGENT=true spelunk search "<topic>"                          # semantic (requires server)
AGENT=true spelunk search "<topic>" --graph                  # semantic + call graph
AGENT=true spelunk memory search "<topic>"                   # search prior decisions

# Optional: If your project is indexed
spelunk search "<topic>" --budget 4000                        # fit within token limit
spelunk explore "question about code"                        # LLM-powered synthesis

# After changes — verify call graph integrity
spelunk graph <symbol> --kind calls
spelunk index .                                              # only if using semantic search

# Session end — store decisions for next session
spelunk memory add --title "Decision: ..." --kind decision
spelunk memory add --title "Handoff: ..." --kind handoff
```
