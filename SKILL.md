# spelunk — AI Agent Skill Reference

spelunk is a **context retrieval tool** for AI agents. Use it to find relevant
code and prior decisions, then reason over the results yourself.

---

## Setup

- `spelunk` (and `spelunk-server`) in PATH

Core features (memory, full-text and ast-grep search, code graph, conventions) work without any inference server.

**Semantic search and AI features** go through `spelunk-server`, which is autostarted locally on demand from v0.8.0. It bundles a native embedder (codefuse-ai/F2LLM-v2-330M, 896-dim, GPU-accelerated on macOS); the embedding model and its compute path are both pinned product-wide, with no external embedding endpoint or config option. Manage it with `spelunk server start|stop|status|logs`. Commands that need the server are marked **(requires server)** below; with `SPELUNK_NO_SERVER=1` they fall back to text/ast-grep search or error clearly.

---

## Code search

```bash
# Full-text search — no server needed
spelunk search "<query>" --mode text

# Call/import graph — no server needed
spelunk graph <symbol-or-file>
spelunk graph <symbol> --kind calls       # calls | imports | extends | implements
spelunk graph <file> --format text|json|jsonl

# Semantic search — (requires server + index)
spelunk search "<query>"
spelunk search "<query>" --limit 20
spelunk search "<query>" --graph          # include call-graph neighbours
spelunk search "<query>" --format text|json|jsonl

# Deep search — iterative, uses LLM (requires server with an LLM backend)
spelunk explore "<question>"
spelunk explore "<question>" --max-steps 5
spelunk explore "<question>" --format json   # {answer, sources, steps}

# Status and checks
spelunk status --format text|json|jsonl
spelunk check --format text|json|jsonl

# Inspect what was indexed for a file
spelunk chunks <file-path>
spelunk chunks <file-path> --format text|json|jsonl
```

Use `search --mode text` for targeted lookups without a server. Use semantic `search` (with server) for concept-level queries. Use `explore` when the answer requires tracing across multiple files — it runs autonomously and reports back.

---

## Indexing

Indexing parses and chunks the source tree (no server needed) and embeds chunks
for semantic search (the embed phase uses the server). Skip embeddings if you
only need full-text/ast-grep search, memory, or the code graph.

```bash
spelunk index <path>           # index (subsequent runs are incremental, blake3-gated)
spelunk index <path> --force   # full re-index (after changing embedding model)
spelunk check                  # verify the index is fresh before starting work
```

Add a `.spelunkignore` file (same syntax as `.gitignore`) to exclude paths from indexing. Takes higher precedence than `.gitignore`. Indexing also applies a built-in filter that skips generated, vendored, minified, and machine-data files (lockfiles, `node_modules/`, `*.min.js`, protobuf codegen, self-declared `@generated`); override it with the `[index]` table in config.

---

## Server daemon

```bash
spelunk server start           # start the local daemon (idempotent; auto-binds 127.0.0.1:7777)
spelunk server status          # PID, port, instance id, uptime
spelunk server logs            # last 50 lines of the server log
spelunk server stop            # stop the daemon (SIGTERM)
```

State lives under `~/.local/state/spelunk/` (`server.pid`, `server.port`, `server.log`).

---

## Plumbing commands

Plumbing commands emit JSONL and are designed for scripts and pipelines.
Exit codes: `0` = success, `1` = no results, `2` = error. See [Plumbing and Porcelain](docs/plumbing-and-porcelain.md) for full details.

```bash
# Parse a file and emit AST chunks (no DB, no server)
spelunk plumbing parse-file <file>

# Compute and verify file hash (no server)
spelunk plumbing hash-file <file>

# Emit code graph edges (no server)
spelunk plumbing graph-edges --file <f> | --symbol <s>

# Emit memory entries as JSONL (no server)
spelunk plumbing read-memory [--kind <k>] [--limit N]

# Emit indexed chunks for a file (requires index)
spelunk plumbing cat-chunks <file>

# List all indexed files (requires index)
spelunk plumbing ls-files [--prefix <p>] [--stale]

# Read embedding from stdin, return nearest chunks by similarity (requires server + index)
echo "your query" | spelunk plumbing embed --query | spelunk plumbing knn --limit 10
```

---

## Memory

Stores decisions, context, and requirements that persist across sessions.
Answers "why was this built this way?" alongside the code index.

### Add an entry

```bash
spelunk memory add \
  --kind decision \
  --title "Chose sqlite-vec over Qdrant" \
  --body "Keeps spelunk self-contained; no external process. Revisit if >1M chunks." \
  --tags "architecture,storage" \
  --files "src/storage/db.rs"

# Supersede an old entry (archives the old one; creates a supersedes edge)
spelunk memory add --kind decision --title "New auth approach" --body "..." \
  --supersedes <old-id>

# Link two entries as related (creates a relates_to edge)
spelunk memory add --kind note --title "Follow-up observation" --body "..." \
  --relates-to <other-id>
```

**Kinds:** `decision` · `context` · `requirement` · `note` · `intent` · `answer` · `handoff` · `question` · `antipattern`

By default (`store_in_git_notes = true`) `memory add` also writes the entry to
`refs/notes/spelunk` on `HEAD`, so memory travels with the code. Graceful no-op
outside a git repo.

To check those notes by hand with stock git, point it at the `spelunk` ref.
Plain `git notes show` reads git's default `commits` ref and reports "no note
found", which is a false negative:

```bash
git notes --ref=spelunk show HEAD    # notes on the current commit
git notes --ref=spelunk list         # every commit carrying spelunk notes
# equivalently
GIT_NOTES_REF=refs/notes/spelunk git notes show HEAD
```

### Query

```bash
spelunk memory search "<question>"        # semantic search over stored entries
spelunk memory search "<q>" --expand-graph  # also include 1-hop relates_to neighbours
spelunk memory list                       # recent entries
spelunk memory list --kind decision       # filter by kind
spelunk memory list --kind decision --limit 10
spelunk memory list --as-of 2026-01-01   # point-in-time snapshot
spelunk memory show <id>                  # full entry + relationships
spelunk memory graph <id>                 # relationship graph for an entry
spelunk memory timeline "<topic>"         # topic evolution across all entries (ASC time)
spelunk memory since <epoch>              # poll for entries newer than Unix timestamp
spelunk memory watch                      # stream new entries as they arrive (SSE; requires a configured server_url)
spelunk memory search "<q>" --format json
spelunk memory failures                   # list all antipatterns (shortcut for list --kind antipattern)
spelunk memory failures --limit 30
```

### Harvest from git history or Claude Code history

```bash
spelunk memory harvest                    # analyse HEAD~10..HEAD
spelunk memory harvest --git-range v0.1.0..HEAD
spelunk memory harvest --branch main      # full branch history
spelunk memory harvest --source claude-code --confirm  # extract from ~/.claude/history.jsonl
spelunk memory harvest --source failures  # extract antipatterns from revert/bugfix commits
spelunk memory harvest --source failures --git-range v0.4.0..HEAD
```

Extracts decisions, requirements, and non-obvious notes. From git, analyzes commit messages.
From `claude-code`, reads agent session transcripts from `~/.claude/history.jsonl`.
Run at the start of a session on a new repo, or after a batch of significant commits.
Requires `llm_model` in config. The `--source claude-code` requires `--confirm` flag.

---

## Status & registry

```bash
spelunk status                 # index health for current project
spelunk status --all           # all registered projects
spelunk status --list          # one-line table
spelunk status --format json   # machine-readable output

spelunk check                  # verify index is fresh; shows active intents and file-overlap warnings
spelunk check --format json    # machine-readable output

spelunk autoclean              # remove stale registry entries (deleted/moved projects)
spelunk link <path>            # include another project's index in searches
spelunk unlink <path>
```

---

## Git worktrees

Read/query commands (`context`, `check`, `search`, `memory list`,
`memory search`, `graph`, `status`) run from a linked worktree resolve to the
main worktree's shared index automatically, with no setup step. Nothing is
written into the worktree:

```bash
git worktree add ../my-feature my-feature-branch
cd ../my-feature
spelunk context    # resolves to the main worktree's index; no init needed
```

`memory add` is a write, not a read/query command, but it resolves the same
way: an entry recorded from a linked worktree lands in the main worktree's
shared `<main-worktree>/.spelunk/memory.db`, and its git-notes write-through
appends to the repo's shared `refs/notes/spelunk`. There is no separate
per-worktree memory store, so recording memory from a worktree needs no setup
and stays in one place.

`spelunk index .` from a worktree is optional. Run it only to refresh the
shared index with files you changed in that worktree; it re-indexes into the
shared `<main-worktree>/.spelunk/index.db`.

`spelunk autoclean` prunes stale registry entries (e.g. after a worktree or
project directory is removed). It does not write to or clean anything inside
the worktree.

---

## Agent mode

Set `AGENT=true` for clean machine-readable output on all commands:

```bash
AGENT=true spelunk search "authentication flow"
AGENT=true spelunk memory search "storage decisions"
AGENT=true spelunk graph src/storage/db.rs
```

---

## Agent workflow

**Start of every session:**
```bash
# Agent entry point — pulls all prior context in one command
AGENT=true spelunk context

# Or filter to a specific memory kind
AGENT=true spelunk context --kind decision

# If you've indexed the project: verify the index is fresh
AGENT=true spelunk check
```

`spelunk context` replaces the multi-command sequence. It retrieves handoffs, open questions, decisions, and requirements in one call. The default output is compact; pass `--budget <N>` (alias `--max-tokens`) to cap total output at N tokens.

**Understanding code:**
1. `AGENT=true spelunk search "<topic>" --mode text` — full-text search, no server needed
2. `AGENT=true spelunk search "<topic>"` — semantic search (requires server + index)
3. Read reported file/line ranges
4. `AGENT=true spelunk graph <symbol>` — trace call chains
5. `AGENT=true spelunk memory search "<topic>"` — check recorded context for *why*

**Making changes:**
1. Search and read before changing
2. Store significant decisions: `spelunk memory add --kind decision …`
3. Store constraints the human states: `spelunk memory add --kind requirement …`
4. After committing (if indexed): `spelunk index <project-root>`

**End of session:**
```bash
spelunk memory add --kind handoff --title "Handoff: <summary>" \
  --body "what's done, what's next, open questions"
spelunk index .   # only if project is indexed
```

**Writing good memory entries:**
- **Title**: one sentence — past tense for decisions, present tense for context
- **Body**: include *why*, what alternatives were rejected, what breaks if ignored
- **Tags**: keep consistent so `list --kind decision` stays useful
- **Files**: link affected files so entries surface in related searches

---

## Tips

- Memory and code graph commands work from any subdirectory — no server or index needed.
- All indexed-project commands can be run from any subdirectory — the index is found automatically.
- `spelunk search --mode text` and `--mode ast-grep` are always available. Semantic `spelunk search` (the `auto` default when an index + server exist) requires the server and a built index. In `ast-grep` mode (and the `auto` fallback with no index) a plain-string query is a case-insensitive substring match (so `Billing` finds `BillingEntity`); a query with a metavariable (`$X`, `$$$ARGS`) matches structurally.
- `spelunk explore`, `spelunk memory harvest`, and LLM summaries require a server with an LLM backend configured.
- After changing the embedding model, run `spelunk index <path> --force` to rebuild the index.
