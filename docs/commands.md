# Commands Reference

Every command accepts `-c, --config <path>` to override the default config file
(`~/.config/spelunk/config.toml`). The flags and defaults below match the
installed binary; run `spelunk <command> --help` to confirm against your version.

A local `spelunk-server` is autostarted on demand and provides embeddings
(native, via the candle-served F2LLM-v2-330M model) and, when a chat model is
configured, LLM inference. Commands that need semantic search or an LLM (`search`
in semantic/auto mode, `explore`, `memory harvest`) use that server; the
always-available commands (`graph`, text/ast-grep `search`, `memory add/list`,
`context`) work with no server.

---

## spelunk init

Initialise spelunk for the current project: register it, parse and chunk the
source tree, start the local server if needed, embed the code, and (when inside
a git repo with an `origin` remote) configure the fetch refspec so project
memory notes travel automatically on `git fetch`.

```
spelunk init [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--hook` | false | Also install the post-commit git hook |
| `--no-index` | false | Skip the initial index run |
| `--name <slug>` | derived | Explicit project slug. Overrides the git-derived default; use it for projects without a git remote. |

`init` writes the project slug to `.spelunk/config.toml` (committed, so the whole
team shares one identity). The slug defaults to the git-derived value:
`host/owner/repo` when an `origin` remote exists, else `local/<blake3-hex>` of the
canonical path. Pass `--name` to set an explicit slug for a repo without a remote
or to choose your own. An existing `project_id` in config is never rewritten, so
re-running `init` (or running it after a rename) does not change an established
slug.

**Memory notes travel with the repository:** When run inside a git repo with an
`origin` remote, `init` configures `remote.origin.fetch` so teammates'
`refs/notes/spelunk` arrives on `git fetch`, landing on the tracking ref
`refs/notes/origin/spelunk`. `memory list`, `context`, and `init` merge that
tracking ref into your own notes, so *reading* teammates' memory needs no extra
step. *Publishing* yours is still manual: the init output includes the push
command; re-run it after each memory change so new notes commits travel. See
[Sharing memory across clones via git-notes](memory.md#sharing-memory-across-clones-via-git-notes).

**Memory survives history rewrites:** `init` also points `notes.rewriteRef` at
`refs/notes/spelunk` in the repository's own config, so memory attached to a
commit is carried onto its replacement by `git commit --amend` and `git rebase`
rather than orphaned on the old sha. This runs even without an `origin`, since
rewrites are local. Note that `git merge --squash` and cherry-picking onto a
divergent base still do not carry notes. See [Surviving history
rewrites](memory.md#surviving-history-rewrites).

If the repo already carries memory on `refs/notes/spelunk`, `init` also hydrates
the new `memory.db` from those notes: every entry not already present is imported
(idempotent, no embeddings), and it prints `Memory:  imported N entries from git
notes` when any were imported. See [Project memory](memory.md) for details.

**Example:**

```bash
cd /path/to/project
spelunk init
spelunk init --hook            # also wire up auto-index/harvest on commit
spelunk init --name acme/tools # explicit slug (e.g. no git remote)
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
| `--batch-size <n>` | 0 (auto) | Cap on the embedding batch size (chunks per server request); the embed phase calibrates the actual size from measured throughput, up to this cap. 0 leaves the cap at the server's own 256-chunk limit |
| `--force` | false | Force full re-index (ignore change detection) |
| `--recount` | false | Backfill `token_count` for existing chunks and exit |
| `--no-summaries` | false | Skip LLM summary generation even when `llm_model` is configured |
| `--summary-batch-size <n>` | 10 | Chunks per LLM summary request |
| `--detach` | false | Re-exec in the background and return immediately (used by git hooks) |
| `--detach-embed` | false | Parse in the foreground, then run the embedding phase in a background process and return the prompt |

A plain `spelunk index` (no `--force`) re-indexes changed files (blake3 hash)
and also backfills embeddings for any already-parsed chunk that has no embedding
yet – for example if a previous run parsed the tree before the embedder had
finished loading. Unchanged, already-embedded files are skipped, so you no
longer need `--force` just to fill in missing embeddings.

Summaries are the exception: a chunk whose summary failed (say the LLM was
unreachable) is recorded as attempted rather than missing, so a plain re-run
skips it. Use `--force` to retry those.

The embed phase calibrates its own batch size instead of guessing: it times a
1-chunk request, then a 4-chunk request, and sizes subsequent requests (and
their timeouts) from the observed per-chunk rate — smaller batches on slow
hardware, larger ones (up to 256 chunks, or your `--batch-size` cap if lower)
on fast hardware. It keeps re-measuring as the run progresses, so a rate that
drifts partway through is picked up rather than locked to the first sample.
Each batch is written to the database as soon as it completes, so an
interrupted run (timeout, machine sleep, process kill) never loses
already-embedded chunks — re-run `spelunk index` to pick up where it left off.

`--detach-embed` is useful when embedding a large codebase on slow hardware:
parsing finishes in the foreground (so the index is immediately usable for
text and ast-grep search) and the long embedding pass continues in the
background. Run `spelunk status` afterwards to check progress; it shows an
"Embedding in progress" line with the embedded/total count until every chunk
is embedded. If the background pass is interrupted, re-running `spelunk index`
resumes it (already-embedded chunks are skipped).

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

`semantic`/`hybrid` uses LinearRAG: a two-stage entity-activation + personalised
PageRank pipeline that improves multi-hop recall over raw KNN. `text` and
`ast-grep` need no embedding model or server. `text` does run over the FTS
index, so it needs `spelunk index` first; `ast-grep` and the `auto` default
work with no index at all.

In `ast-grep` mode (and the `auto` fallback used when there is no index or
server), a plain-string query matches case-insensitively as a substring of
identifiers and file text, so `Billing` finds `BillingEntity`. A query
containing a metavariable (`$X`, `$$$ARGS`) is instead compiled as a structural
ast-grep pattern. Neither needs an index. This is literal substring matching,
not semantic search (that needs the server).

**Example:**

```bash
spelunk search "where is the JWT token validated"
spelunk search "database schema migration" --limit 5 --format json
spelunk search "authentication middleware" --graph
spelunk search "TODO fix me" --mode text         # FTS only, no server needed
spelunk search "Billing" --mode ast-grep         # case-insensitive substring, no index
spelunk search "$X.unwrap()" --mode ast-grep     # structural pattern (metavariable)
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

When chunks outnumber embeddings, `spelunk status` prints an "Embedding in
progress" line showing the embedded/total count. This covers both an active
background embed (e.g. `spelunk index --detach-embed` still running) and an
interrupted run that can be resumed with `spelunk index`.

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
| `-l, --limit <n>` | per-section | Max entries per section (handoff=3, question=10, decision=10, requirement=10); mutually exclusive with `--budget` |
| `--budget <n>` (alias `--max-tokens`) | unlimited | Cap total output to this many tokens; mutually exclusive with `--limit` |
| `--path <path>` | — | Only show entries tagged with this file/directory |
| `--format text\|json` | text | Output format |
| `--no-conventions` | false | Skip the conventions section |
| `--local-only` | false | Skip cross-project dep pass; query only the primary project's memory |

Under a tight `--budget`, durable memory (decisions and requirements) is kept
ahead of ephemeral open questions when trimming to fit; the section display
order is unchanged.

When projects are linked with `spelunk link`, `context` also surfaces `locked`
or `cross-project`-tagged `decision` and `requirement` entries from linked
projects' memory stores, each labelled with its source project. Pass
`--local-only` to suppress this behaviour. See [Memory](memory.md#cross-project-visibility).

**Example:**

```bash
spelunk context
spelunk context --kind decision
spelunk context --local-only      # primary project only, no dep pass
spelunk context --budget 4000     # cap total output at ~4000 tokens
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
| `--live` | false | Skip the index and scan live files directly |

The live scan is structural and matches only call-site `symbol(...)`
invocations, so a zero result means "no bare calls", not "unused". Class,
constant, association, and receiver-method references never take that form. Run
`spelunk init` to build the full graph, which adds imports/extends/implements
edges alongside call edges.

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

Manage spelunk's git hooks.

```
spelunk hooks install [--ci]
spelunk hooks install --pre-push
spelunk hooks uninstall
```

`install` writes a post-commit hook that runs `spelunk index` and
`spelunk memory harvest` after each commit (both `--detach` so git is not
blocked). Developers without `spelunk` installed are unaffected. `--ci` prints a
GitHub Actions workflow step instead of writing a hook.

`install --pre-push` writes a pre-push hook that publishes your memory
(`refs/notes/spelunk`) to the remote you are pushing to, so decisions travel with
the code they describe. It merges the remote's notes into yours before pushing (a
union, so neither side is dropped) and retries a lost race up to three times. It
never blocks your push: on failure it warns on stderr and exits 0, and it never
force-pushes. Publishing is opt-in, so your memory stays local until you install
it. See [memory.md](memory.md#sharing-memory-across-clones-via-git-notes).

Neither hook overwrites one it did not write: if a hook of that name already
exists, `install` reports it and leaves the file alone.

`uninstall` removes every hook spelunk installed, leaving any other hooks alone.

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
| `start` | Idempotent; binds `--port` exactly (default 7777) on `127.0.0.1`. Reclaims a wedged prior daemon of ours instead of drifting to a new port; fails loudly if an unrelated process holds the port. A single-instance guard refuses a second server against a different `server.db`. |
| `stop` | Graceful SIGTERM, then SIGKILL escalation for an unresponsive daemon; reports success only once the process is confirmed gone. |
| `status` | Print PID, port, instance id, and uptime |
| `logs` | Print the last N lines of the server log (`-n`, default 50) |

```bash
spelunk server start
spelunk server status
spelunk server logs -n 100
spelunk server stop
```

---

## spelunk login

Authenticate with spelunk.cloud using a browser-based device login. `spelunk
login` prints a verification URL and a short user code; open the URL, enter the
code, and approve the sign-in in your browser. On success, short-lived tokens
are stored in your config and refreshed automatically in the background, so you
do not need to log in again until the refresh token expires.

```
spelunk login [--org <slug>] [--cloud-url <url>]
```

| Flag | Notes |
|------|-------|
| `--org <slug>` | After the device login yields a token, silently re-scope the session to this org (login-then-switch). If you are already logged in with a stored refresh token, re-scopes without a new device login. Multi-org accounts choose their org on the browser-hosted approval page during the device flow itself. |
| `--cloud-url <url>` | Override the cloud API URL (default `https://api.spelunk.cloud`; also settable via `SPELUNK_CLOUD_URL`). |

```bash
spelunk login
spelunk login --org acme
```

**No `--org`, and the device login itself didn't scope you to an org** (WorkOS
doesn't auto-select an org even for single-org accounts): `spelunk login`
resolves one for you instead of leaving a session that needs a follow-up
`spelunk org switch`.

- Exactly one org on your account → selected silently.
- Multiple orgs, on a TTY → an interactive `name (slug)` selector.
- Multiple orgs, non-TTY (CI/agent shell) → errors with an actionable "pass
  `--org <slug>`" message and a non-zero exit; never hangs on a prompt.
- Zero orgs → a clear onboarding message and a non-zero exit; no dangling
  no-org session is persisted.

Tokens are written to the `[auth]` table of `~/.config/spelunk/config.toml`
(file mode `0600`). Existing setups that use a static `server_key` (or the
`SPELUNK_SERVER_KEY` environment variable) keep working unchanged until you next
run `spelunk login`; `SPELUNK_SERVER_KEY` continues to take precedence, which is
handy for CI.

### Where the `server_key` credential is stored

The static `server_key` bearer credential is **not** kept in plaintext in
`config.toml`. It lives in your operating system's secret store:

- **macOS** — Keychain
- **Linux** — Secret Service (libsecret / `org.freedesktop.secrets`)
- **Windows** — Credential Manager

The first time you run any command after upgrading, a `server_key` previously
written to `~/.config/spelunk/config.toml` is migrated into the OS keychain and
removed from the file automatically — no action required. (A shared
`server_key` set in a project's checked-in `.spelunk/config.toml` is left as-is;
it is a team key by design, not a personal credential.)

**Headless / CI / containers.** When no OS keychain backend is available, the
credential never causes a hard failure:

- `SPELUNK_SERVER_KEY` remains the non-interactive escape hatch and always takes
  precedence — set it in CI and you never touch the keychain.
- Otherwise spelunk falls back to an owner-only (`0600`) file at
  `~/.config/spelunk/secrets.toml`.

`SPELUNK_SECRET_STORE` pins the backend explicitly:

| Value | Behaviour |
|-------|-----------|
| unset / `auto` | Prefer the OS keychain; fall back to the file store when none is available (default). |
| `keychain` | Require the OS keychain; error if it is unavailable. |
| `file` | Always use the `secrets.toml` file store (e.g. a container that mounts secrets from elsewhere). |

The credential is never logged.

---

## spelunk org

Manage the active organization for an authenticated session.

```
spelunk org switch <slug|uuid>
```

`spelunk org switch` re-scopes your session to another organization you belong
to, reusing the stored credentials — no new device login is required. Accepts an
org slug or its UUID.

```bash
spelunk org switch acme
```

---

## spelunk logout

Remove stored spelunk.cloud credentials. Clears the `[auth]` tokens written by
`spelunk login` from `~/.config/spelunk/config.toml`, the `server_key` from the
OS keychain (or `secrets.toml` fallback), and any legacy plaintext `server_key`
still left in `config.toml`.

```
spelunk logout
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
spelunk memory push                         # one-way: push local entries to the configured server
spelunk memory pull                         # one-way: pull new server entries into local memory.db
spelunk memory sync                         # two-way: push local + pull remote (see `spelunk sync`)
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

**Entry identity:** entries are identified by a SHA-256 over exactly their
`kind`, `title`, and `body`, so the same decision recorded on two machines
converges on one identity. `memory reconcile` and the `spelunk init` git-notes
import dedup on it: entries with identical text collapse into one even when
their creation time, tags, or linked files differ, and the survivor carries the
union of the tags and linked files. The `id` shown by `memory list` is a local
row number, not this identity. See [Entry identity](memory.md#project-memory).

---

## spelunk sync

Two-way sync (shorthand for `spelunk memory sync`): push your local memory
entries to the configured server **and** pull remote entries into the local
`memory.db`, so a team converges on one shared memory. Code never leaves the
machine; only memory does. Requires a configured `server_url`.

```
spelunk sync [--project <slug>] [--source <path>] [--include-archived]
```

| Flag | Notes |
|------|-------|
| `--project <slug>` | Project slug to sync into. Required on first sync when no `project_id` is configured: the server lazily creates the project from this slug, and repeat syncs with the same slug reuse it. Overrides a configured `project_id` when both are present. Never auto-derived from the folder name or git remote; with neither flag nor a configured `project_id`, sync halts with an actionable message pointing at `--project`. |
| `--source <path>` | Local `memory.db` to sync (default: the auto-detected project `memory.db`). |
| `--include-archived` | Include archived entries in the push, propagating tombstones. |

For a one-directional transfer, use `spelunk memory push` (local → server) or
`spelunk memory pull` (server → local).

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
| `SPELUNK_CLOUD_URL` | Override the spelunk.cloud API URL used by `login` / `org` (default `https://api.spelunk.cloud`) |
| `SPELUNK_SERVER_KEY` | Static credential for a team/self-hosted server; takes precedence over the keychain-stored credential and `login` tokens (the non-interactive escape hatch for CI / headless) |
| `SPELUNK_SERVER_CA` | Path to a PEM CA bundle to trust for a `SPELUNK_SERVER_URL` whose certificate is signed by an internal or self-signed CA. Added as a trust anchor on top of the built-in roots; TLS verification stays on (no insecure mode). Overrides `server_ca` in `config.toml`. |
| `SPELUNK_SECRET_STORE` | Secret-store backend: `auto` (default — keychain, file fallback), `keychain` (require the OS keychain), or `file` (force `~/.config/spelunk/secrets.toml`) |
| `RUST_LOG=debug` | Enable verbose logging |
| `EDITOR` / `VISUAL` | Editor opened by `spelunk memory add` when `--body` is omitted |
