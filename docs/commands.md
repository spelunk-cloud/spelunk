# Commands Reference

Every command accepts `-c, --config <path>` to override the default config file
(`~/.config/spelunk/config.toml`). Flags and defaults below are taken from the
v0.8.0 binary (`spelunk <command> --help`).

From v0.8.0 a local `spelunk-server` is autostarted on demand and provides
embeddings (native, via fastembed-rs) and — when a chat model is configured —
LLM inference. Commands that need semantic search or an LLM (`search` in
semantic/auto mode, `explore`, `memory harvest`) use that server; the
always-available commands (`graph`, text/ast-grep `search`, `memory add/list`,
`context`) work with no server.

---

## spelunk init

Initialise spelunk for the current project: register it, parse and chunk the
source tree, start the local server if needed, and embed the code.

```
spelunk init [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--hook` | false | Also install the post-commit git hook |
| `--no-index` | false | Skip the initial index run |

**Example:**

```bash
cd /path/to/project
spelunk init
spelunk init --hook        # also wire up auto-index/harvest on commit
```

---

## spelunk index

Index a codebase directory.

```
spelunk index <path> [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-d, --db <path>` | auto | Override database path |
| `--batch-size <n>` | 32 | Max concurrent embedding requests |
| `--force` | false | Force full re-index (ignore change detection) |
| `--recount` | false | Backfill `token_count` for existing chunks and exit |
| `--no-summaries` | false | Skip LLM summary generation even when `llm_model` is configured |
| `--summary-batch-size <n>` | 10 | Chunks per LLM summary request |
| `--detach` | false | Re-exec in the background and return immediately (used by git hooks) |

Add a `.spelunkignore` file (same syntax as `.gitignore`) to any directory to
exclude files from indexing. It takes higher precedence than `.gitignore`.

**Example:**

```bash
spelunk index ./myproject
spelunk index ./myproject --force --batch-size 16
```

---

## spelunk search

Search the index. In `auto` mode (the default) spelunk uses semantic/hybrid
search when an index and server are available and silently falls back to
ast-grep otherwise.

```
spelunk search <query> [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-l, --limit <n>` | 10 | Number of results (max 100); mutually exclusive with `--budget` |
| `--budget <n>` | — | Return best chunks fitting within this token budget |
| `--format text\|json\|jsonl` | text | Output format |
| `-g, --graph` | false | Enrich results with 1-hop call-graph neighbours |
| `--graph-limit <n>` | 10 | Max graph-expanded results to add (with `--graph`) |
| `--mode <mode>` | auto | `auto`, `text` (FTS only), `semantic`/`hybrid` (LinearRAG), or `ast-grep` |
| `-d, --db <path>` | auto | Override database path |
| `--no-stale-check` | false | Suppress the stale-index warning |
| `--local-only` | false | Search only the primary index, skip linked projects |
| `--as-of <sha>` | — | Search a snapshot at a commit SHA instead of the live index |

`semantic`/`hybrid` uses LinearRAG: a two-stage entity-activation + personalised
PageRank pipeline that improves multi-hop recall over raw KNN. `text` and
`ast-grep` need no embedding model or server.

**Example:**

```bash
spelunk search "where is the JWT token validated"
spelunk search "database schema migration" --limit 5 --format json
spelunk search "authentication middleware" --graph
spelunk search "TODO fix me" --mode text         # FTS only, no server needed
spelunk search "fn .*token.*\(" --mode ast-grep  # structural live search
```

---

## spelunk explore

Agentic search: the server's LLM iteratively calls spelunk's own tools (search,
graph, read) to answer an open-ended question. Requires a server with an LLM
backend configured.

```
spelunk explore "<question>" [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--max-steps <n>` | 10 | Stop after this many tool-call steps |
| `--verbose` | false | Print each tool call and result to stderr |
| `--format text\|json` | text | Output format (`json` emits `{answer, sources, steps}`) |
| `-d, --db <path>` | auto | Override database path |

**Example:**

```bash
spelunk explore "how does incremental indexing work?"
spelunk explore "what guards the context window in the LLM pipeline?" --verbose
AGENT=true spelunk explore "where is authentication enforced?" --format json
```

**Security note:** the loop's `read_file` tool can only return content from
files that are part of the index, resolved relative to the project root.
Absolute paths, `..` traversals, and any path outside the indexed project are
denied, so adversarial instructions hidden in indexed source cannot steer the
LLM into reading files such as `~/.ssh/id_rsa`. A denied read is reported back
to the loop without exposing a resolved path or file contents.

---

## spelunk status

Show indexing statistics for the current project (or all projects).

```
spelunk status [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-a, --all` | false | Show all registered projects |
| `-l, --list` | false | One-line-per-project format (implies `--all`) |
| `--format text\|json` | text | Output format |

**Example:**

```bash
spelunk status
spelunk status --all --format json
```

---

## spelunk check

Check whether the index is in sync with the source tree. Exits with code 1 if
the index is stale.

```
spelunk check [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--format text\|json\|porcelain` | text | Output format |
| `--files` | false | Also list the stale file paths (one per line) |
| `-d, --db <path>` | auto | Override database path |

```bash
spelunk check || echo "Index is stale — run spelunk index"
spelunk check --format porcelain --files
```

---

## spelunk context

Print agent session context: handoffs, open questions, decisions, requirements,
and (when an index is available) extracted conventions. This is the recommended
entry point for an agent starting a session.

```
spelunk context [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--db <path>` | auto | Override the memory database path |
| `--index-db <path>` | auto | Index DB used to load the conventions section |
| `--backend sqlite\|git-notes` | sqlite | Memory storage backend |
| `-k, --kind <kind>` | — | Filter to a single kind instead of the multi-section view |
| `-l, --limit <n>` | per-section | Max entries per section (handoff=3, decision=10, question/requirement=500) |
| `--path <path>` | — | Only show entries tagged with this file/directory |
| `--format text\|json` | text | Output format |
| `--no-conventions` | false | Skip the conventions section |
| `--local-only` | false | Skip cross-project dep pass; query only the primary project's memory |

When projects are linked with `spelunk link`, `context` also surfaces `locked`
or `cross-project`-tagged `decision` and `requirement` entries from linked
projects' memory stores, each labelled with its source project. Pass
`--local-only` to suppress this behaviour. See [Memory](memory.md#cross-project-visibility).

**Example:**

```bash
spelunk context
spelunk context --kind decision
spelunk context --local-only      # primary project only, no dep pass
AGENT=true spelunk context        # JSON for machine processing
```

---

## spelunk graph

Query the code graph: imports, function calls, class inheritance.

```
spelunk graph <symbol> [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--kind <type>` | all | Filter: `imports`, `calls`, `extends`, `implements` |
| `--format text\|json\|jsonl` | text | Output format |
| `-d, --db <path>` | auto | Override database path |
| `--no-stale-check` | false | Suppress the stale-index warning |
| `--live` | false | Skip the index and scan live files with ast-grep (requires `ast-grep` in PATH) |

**Example:**

```bash
spelunk graph RagPipeline
spelunk graph src/storage/db.rs --kind imports
spelunk graph validate_token --live
```

---

## spelunk chunks

Show the raw indexed chunks for a file. Useful for debugging or providing
precise context to an agent.

```
spelunk chunks <path> [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--format text\|json\|jsonl` | text | Output format |
| `-d, --db <path>` | auto | Override database path |

```bash
spelunk chunks src/indexer/parser.rs
spelunk chunks src/indexer/parser.rs --format json
```

---

## spelunk languages

List all supported languages and their tree-sitter parsers.

```
spelunk languages
```

---

## spelunk link / spelunk unlink / spelunk links

Add or remove a project dependency. When linked, `spelunk search` also queries
the linked project's index, and `spelunk memory search|list|context` surfaces
`locked`/`cross-project`-tagged decisions and requirements from the linked
project's memory store. `spelunk links` inspects existing links.

```
spelunk link <path>
spelunk unlink <path>
spelunk links list      # list all linked projects with status
spelunk links check     # exit 1 if any linked index is stale or missing
```

```bash
spelunk link ../shared-utils       # search this project and shared-utils together
spelunk links list
```

---

## spelunk autoclean

Remove registry entries for projects whose root path no longer exists on disk.

```
spelunk autoclean
```

---

## spelunk hooks

Manage git post-commit hooks.

```
spelunk hooks install [--ci]
spelunk hooks uninstall
```

`install` writes a post-commit hook that runs `spelunk index` and
`spelunk memory harvest` after each commit (both `--detach` so git is not
blocked). Developers without `spelunk` installed are unaffected. `--ci` prints a
GitHub Actions workflow step instead of writing a hook.

---

## spelunk server

Manage the local `spelunk-server` daemon. Runtime state lives under
`~/.local/state/spelunk/` (`server.pid`, `server.port`, `server.log`).

```
spelunk server start [--port <n>] [--bin <path>] [--db <path>]
spelunk server stop
spelunk server status
spelunk server logs [-n <lines>]
```

| Subcommand | Notes |
|------------|-------|
| `start` | Idempotent; tries `--port` (default 7777) then 7778–7787 on collision; auto-binds `127.0.0.1` |
| `stop` | SIGTERM the running daemon and wait for exit |
| `status` | Print PID, port, instance id, and uptime |
| `logs` | Print the last N lines of the server log (`-n`, default 50) |

```bash
spelunk server start
spelunk server status
spelunk server logs -n 100
spelunk server stop
```

---

## spelunk memory

Store and query project context, decisions, and requirements. See
[Memory](memory.md) for full documentation.

```
spelunk memory add --title "..." [--body "..."] [--kind decision] [--tags auth,db] [--files src/auth.rs]
spelunk memory add --from-url <url> [--title "override"] [--kind requirement]
spelunk memory search <query> [--limit 10] [--format text|json] [--local-only]
spelunk memory list [--kind decision] [--limit 20] [--format text|json] [--local-only]
spelunk memory show <id> [--format text|json]
spelunk memory harvest [--git-range HEAD~10..HEAD] [--source git|claude-code|failures]
spelunk memory failures                    # list all antipatterns
spelunk memory archive <id>
spelunk memory supersede <id> --title "..." # archive old, add replacement
spelunk memory timeline <topic>
spelunk memory graph <id>
spelunk memory since <unix-ts>
spelunk memory push                         # push local entries to the configured server
spelunk memory watch                        # stream new entries from the server (SSE)
spelunk memory reconcile [--dry-run] [--all-projects] [--source-db <path>]
```

All `memory` subcommands accept `--backend sqlite|git-notes` (default `sqlite`)
and `--db <path>`.

`memory search` and `memory list` accept `--local-only` to skip the
cross-project dep pass (see [Cross-project visibility](memory.md#cross-project-visibility)).
Results from linked projects carry a `[from: <project>]` badge in text output
and `source_project` / `source_project_path` fields in JSON.

**Memory kinds:** `decision` · `context` · `requirement` · `note` · `intent` ·
`answer` · `handoff` · `question` · `antipattern`

`spelunk memory failures` is a shortcut for `spelunk memory list --kind antipattern`.

**git-notes write-through:** when `store_in_git_notes` is true (the default),
`spelunk memory add` also appends the entry to `refs/notes/spelunk` on `HEAD`,
so memory travels with the code. Outside a git repo this is a graceful no-op.

---

## spelunk sync

Alias for `spelunk memory push`: sync local memory entries to the configured
server.

```
spelunk sync
```

---

## spelunk plumbing

Low-level commands for agents and scripts. All emit JSONL and exit non-zero on
error (exit 1 for "no results", exit 2 for errors). See
[plumbing-and-porcelain.md](plumbing-and-porcelain.md).

```
spelunk plumbing cat-chunks <file>     # indexed chunks for a file
spelunk plumbing ls-files              # all indexed files
spelunk plumbing parse-file <file>     # parse + chunk without storing
spelunk plumbing hash-file <file>      # blake3 hash + index currency
spelunk plumbing knn <query>           # KNN vector search
spelunk plumbing embed                 # read stdin lines, emit vectors
spelunk plumbing graph-edges           # code graph edges
spelunk plumbing read-memory           # memory entries as JSONL
```

---

## Environment variables

| Variable | Effect |
|----------|--------|
| `AGENT=true` | Force JSON output for commands that support it |
| `SPELUNK_NO_SERVER=1` | Never autostart or use a server (fully offline / no-server mode) |
| `SPELUNK_SERVER_URL` | Point the CLI at a specific server URL |
| `RUST_LOG=debug` | Enable verbose logging |
| `EDITOR` / `VISUAL` | Editor opened by `spelunk memory add` when `--body` is omitted |
