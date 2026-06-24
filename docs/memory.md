# Project Memory

`spelunk memory` is a per-project knowledge store. Use it to capture decisions, context, requirements, questions, and handoff notes that would otherwise live only in chat history or someone's head.

Memory entries are stored in a local SQLite database by default, and — with
`store_in_git_notes` enabled (the default) — also written through to
`refs/notes/spelunk` on `HEAD`, so they travel with the repository. No external
database or server is required. (You can make git-notes the primary backend with
`--backend git-notes`, or point at a shared server with `server_url`.) The auto-started local `spelunk-server` (loopback) is used only for *inference* (embeddings/LLM for semantic search) — it does **not** store memory. Memory lives on a server only when you *explicitly* configure a team `server_url`. Entries
are searchable by full text at all times; semantic search (by meaning) is
available when a server is running — the local one is autostarted on demand.

## Why memory?

Code tells you *what* the system does. Memory tells you *why* it was built that way.

Examples of things worth storing:

- "We chose sqlite-vec over pgvector because the project must run without a Postgres server."
- "The embedding format is `title: {name} | text: {content}` — changing this invalidates all stored embeddings."
- "Current question: should the harvester dedupe by commit SHA or by entry content hash?"
- "Handoff to next session: the graph migration is done, secrets scanner is next."

## Memory kinds

| Kind | Use for |
|------|---------|
| `decision` | Architecture or design choices with rationale |
| `context` | Background information that helps understand the codebase |
| `requirement` | Product or technical requirements |
| `note` | General observations (default) |
| `question` | Open questions that need an answer |
| `answer` | Answers to previously stored questions |
| `handoff` | State transfer between work sessions or agents |
| `intent` | Active work signal; surfaced by `spelunk check` with file-overlap warnings |
| `antipattern` | Things to avoid; list with `spelunk memory failures` |

## Storing a note

```bash
# Quick note with body inline
spelunk memory add --title "Chunker uses 120-line sliding window as fallback" \
              --body "This applies to unsupported file types and binary-adjacent files." \
              --kind context \
              --tags chunker,indexer

# Open your $EDITOR for the body (omit --body)
spelunk memory add --title "Decision: use blake3 for file hashing" --kind decision

# Link to specific files
spelunk memory add --title "Auth middleware refactored" \
              --body "Moved session validation to src/auth/middleware.rs" \
              --files "src/auth/middleware.rs,src/auth/session.rs"

# Record when a decision became valid (ISO 8601)
spelunk memory add --title "Adopted monorepo layout" --kind decision \
              --valid-at 2026-01-15

# Supersede an old entry — archives it and records a supersedes edge
spelunk memory add --title "New auth approach" --kind decision --body "..." \
              --supersedes <old-id>

# Mark two entries as related — creates a relates_to edge
spelunk memory add --title "Follow-up note" --kind note --body "..." \
              --relates-to <other-id>
```

When `--body` is omitted, `spelunk` opens `$VISUAL` or `$EDITOR` (falling back to `vi`). Lines starting with `#` are stripped (comment convention).

## Pulling in context from a URL

`--from-url` fetches content from a GitHub issue, Linear ticket, or any web page and stores it as a memory entry. The title is inferred from the page automatically.

```bash
# GitHub issue — uses `gh api` for clean structured content
spelunk memory add --from-url https://github.com/owner/repo/issues/42

# Override the inferred title
spelunk memory add --from-url https://github.com/owner/repo/issues/42 \
              --title "Auth: session token storage compliance issue" \
              --kind requirement

# Any URL — fetches page title and strips HTML
spelunk memory add --from-url https://linear.app/myteam/issue/ENG-1234/... \
              --kind context

# Combine with tags
spelunk memory add --from-url https://github.com/owner/repo/issues/99 \
              --tags auth,security --kind requirement
```

For GitHub issues, `spelunk` calls `gh api` to get structured issue data (requires the [GitHub CLI](https://cli.github.com/) and `gh auth login`). For all other URLs it does an HTTP GET and extracts readable text.

## Searching memory

```bash
# Semantic search — finds entries by meaning
spelunk memory search "why did we choose sqlite"
spelunk memory search "authentication decisions" --limit 5

# Also surface 1-hop relates_to neighbours of each result
spelunk memory search "authentication decisions" --expand-graph

# Search mode: hybrid (default), semantic, text
spelunk memory search "auth" --mode semantic
spelunk memory search "auth" --mode text

# Point-in-time: only entries that were valid at this date
spelunk memory search "auth decisions" --as-of 2026-01-01
```

## Tracking topic evolution

`spelunk memory timeline` returns all entries related to a topic, sorted by the time they became valid — useful for understanding how a decision or understanding evolved.

```bash
spelunk memory timeline "authentication strategy"
spelunk memory timeline "database choice" --limit 30
spelunk memory timeline "auth" --format json
```

## Listing entries

```bash
# List recent entries (newest first)
spelunk memory list

# Filter by kind
spelunk memory list --kind decision
spelunk memory list --kind question

# More entries
spelunk memory list --limit 50

# Point-in-time snapshot — only entries valid at a given date
spelunk memory list --as-of 2026-01-01

# Filter by commit SHA (exact or prefix)
spelunk memory list --source-ref abc1234
```

`question` and `answer` entries show titles only in list view to avoid context saturation. Use `spelunk memory show <id>` to read the full body.

## Cross-project visibility

When projects are linked with `spelunk link`, `spelunk memory search`,
`spelunk memory list`, and `spelunk context` automatically surface relevant
memory from linked projects alongside local results. This is how settled
decisions recorded in one project (for example, a Cloud-only architecture
constraint in `cloud-api`) remain visible to agents working in a sibling
project (for example, `spelunk-oss`).

### What crosses project boundaries

Not all memory propagates. Only entries that match **all three** of the
following criteria are surfaced from a linked project:

- **Kind:** `decision` or `requirement` (never `handoff`, `question`, or `note`).
- **Tag:** must carry the tag `locked` (for settled v1 decisions) or
  `cross-project` (for cross-cutting items that are not otherwise locked). Tags
  like `auth` or `database` alone are not sufficient.
- **Status:** `active` only. Archived or superseded cross-project decisions do
  not resurface after they are retracted in the source project.

Decisions and requirements that do not carry `locked` or `cross-project` remain
strictly project-local, regardless of which `spelunk link` edges are configured.

### Source attribution

Every result from a linked project is labelled with its origin so conflicting
decisions between projects are visible and attributable:

- **Text output:** a `[from: <project>]` badge appended to the entry line.
- **JSON output:** `source_project` and `source_project_path` fields on the
  note object (absent on local results, so existing JSON consumers are
  unaffected).

Local results always appear first; cross-project results are appended, in
registry dependency order, after all local results. The existing `--limit` flag
applies only to the local query; cross-project results are additional and not
counted against the limit.

### Skipping the dep pass

Pass `--local-only` to any of `memory search`, `memory list`, or `context` to
query only the primary project's memory store:

```bash
spelunk memory search "auth decisions" --local-only
spelunk memory list --kind decision --local-only
spelunk context --local-only
```

### Tagging decisions for cross-project visibility

```bash
# Tag a decision as locked so linked projects can see it
spelunk memory add --kind decision \
  --title "SSE memory stream is Cloud-only" \
  --body "OSS spelunk-server must not expose SSE; Cloud API owns that surface." \
  --tags v1,locked

# Tag a requirement that applies across all linked projects
spelunk memory add --kind requirement \
  --title "All writes validated for secrets before storage" \
  --body "Applies to cloud-api and spelunk-oss alike." \
  --tags security,cross-project
```

### Privacy boundary

The dep pass reads each linked project's `memory.db` directly from disk (local
SQLite only). It does not route through `spelunk-server` or any remote endpoint.
A linked project's memory is only reachable if its `memory.db` file is
accessible on the local filesystem (same machine, same user). Remote or
server-backed linked projects whose memory lives exclusively on a remote server
are not queried by the dep pass in v1.

## Showing a single entry

```bash
spelunk memory show 42
spelunk memory show 42 --format json
```

`memory show` displays the full body plus any incoming and outgoing relationship edges (supersedes, relates_to, contradicts) with linked entry titles.

## Relationship graph

```bash
# Show all edges for an entry (text)
spelunk memory graph 42

# Machine-readable
spelunk memory graph 42 --format json
```

## Harvesting from git history

`spelunk memory harvest` reads your git log, sends commit messages to the LLM, and automatically extracts significant entries. Requires `llm_model` in `~/.config/spelunk/config.toml`.

```bash
# Default: last 10 commits
spelunk memory harvest

# Custom range
spelunk memory harvest --git-range HEAD~30..HEAD
spelunk memory harvest --git-range v1.0..HEAD
```

Already-harvested commits are skipped (tracked via a `git:<sha>` tag). Routine commits ("fix typo", "wip", etc.) are ignored by the LLM.

### Automatic harvesting

Install the git hook and harvesting happens on every commit:

```bash
spelunk hooks install
```

## Importing from a local server

`spelunk memory reconcile` imports notes that were recorded by a running
`spelunk-server` daemon into the project's local `memory.db`. This is useful
after a session where entries were written through `server_url` and need to be
pulled into the project's local store, or when migrating from server-backed to
local storage.

Dedup is by content hash: notes already present in `memory.db` (same kind,
title, body, tags, files, and creation time) are skipped. The source `server.db`
is opened read-only; it is never modified.

```bash
# Import notes for the active project (default source: ~/.local/state/spelunk/server.db)
spelunk memory reconcile

# Preview what would be imported without writing anything
spelunk memory reconcile --dry-run

# Import notes for all projects found in server.db
spelunk memory reconcile --all-projects

# Override the source path
spelunk memory reconcile --source-db /var/run/spelunk/server.db

# Machine-readable summary
spelunk memory reconcile --format json
```

Exit codes: `0` on success or when there is nothing to import, non-zero on
hard errors (unreadable source DB, write failure). When `server.db` does not
exist the command is a no-op and exits 0.

If reconcilable notes are detected at startup, spelunk prints a one-time nudge
to stderr. Set `SPELUNK_NO_RECONCILE_NUDGE=1` to suppress it in CI or scripts.

### Security notes

`reconcile` opens `server.db` with `SQLITE_OPEN_READONLY` and `PRAGMA
journal_mode=WAL` to avoid blocking the daemon's writers. No content from
`server.db` is executed or passed to an LLM; the only write target is the
project's own `memory.db`. Embeddings are re-generated from the imported text
via the configured server (best-effort; notes import successfully even when the
server is unreachable).

## Using memory as context

`spelunk memory search` results are best consumed alongside `spelunk search` results — they answer the *why* while the code search answers the *how*. Pass both to your reasoning model for a complete picture.

## Machine-readable output

All memory commands support `--format json`, and setting `AGENT=true` forces JSON mode globally:

```bash
AGENT=true spelunk memory list --kind question
AGENT=true spelunk memory search "database decisions"
```

## Tips

- **Store the "why", not just the "what"** — the code already captures what was built.
- **Use `question` kind actively** — when you hit a decision point you're unsure about, store it. Come back with `spelunk memory list --kind question` at the start of the next session.
- **Use `handoff` kind** at the end of a long session to summarise the current state for your next session (or for another agent).
- **Tag entries** — tags like `auth`, `database`, `performance` make `spelunk memory list` more scannable and improve search relevance.
- **Use `--supersedes` when updating a decision** — it archives the old entry, sets its invalidation time, and creates a traceable edge so you can always follow the chain of reasoning.
- **Use `--relates-to` for non-superseding connections** — linking a follow-up note or a contradicting observation lets `memory graph` and `--expand-graph` surface related context automatically.
- **Use `--as-of` for archaeology** — `spelunk memory list --as-of 2026-01-01` shows the knowledge state at that date, which is useful for post-mortems or understanding old decisions in context.
