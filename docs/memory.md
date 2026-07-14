# Project Memory

`spelunk memory` is a per-project knowledge store. Use it to capture decisions, context, requirements, questions, and handoff notes that would otherwise live only in chat history or someone's head.

Memory entries are stored in a local SQLite database by default, and — with
`store_in_git_notes` enabled (the default) — also written through to
`refs/notes/spelunk` on `HEAD`, so they travel with the repository. No external
database or server is required. (You can make git-notes the primary backend with
`--backend git-notes`, or point at a shared server with `server_url`.) The auto-started local `spelunk-server` (loopback) is used only for *inference* (embeddings/LLM for semantic search) — it does **not** store memory. Memory lives on a server only when you *explicitly* configure a team `server_url`. Entries
are searchable by full text at all times; semantic search (by meaning) is
available when a server is running — the local one is autostarted on demand.

To verify that memory really travels with the repository, inspect the notes by
hand with stock git. They live on the `spelunk` ref, so you must name it: plain
`git notes show HEAD` reads git's default `commits` ref and reports "no note
found" even when spelunk has written entries.

```bash
git notes --ref=spelunk show HEAD    # notes on the current commit
git notes --ref=spelunk list         # every commit carrying spelunk notes
# equivalently
GIT_NOTES_REF=refs/notes/spelunk git notes show HEAD
```

**Carrier and index.** Think of `refs/notes/spelunk` as the durable *carrier*
for memory and `.spelunk/memory.db` as the queryable *index* built over it. Every
`memory add` appends its entry to the carrier through one write-through path;
`spelunk init` hydrates the index by importing those notes, adding the embeddings
semantic search needs. Both live in the repo, and the store of record stays local
unless you configure a team `server_url`. The carrier reaches teammates only once
the notes ref is pushed and fetched (see [Sharing memory across clones via
git-notes](#sharing-memory-across-clones-via-git-notes) below).

**Entry identity.** An entry is identified by what it says, not by where or when
it was recorded. spelunk derives a canonical identity for every entry as a
SHA-256 over exactly its `kind`, `title`, and `body`. Two people who
independently record the same decision in two clones arrive at the same
identity, with no server and no coordination between them. Mutable metadata is
deliberately excluded, so tagging, archiving, or superseding an entry never
changes its identity. (The numeric `id` in `memory list` output is a local row
number rather than an identity: `spelunk init` renumbers it, and each machine
assigns it independently.)

**Before `spelunk init`**, `memory add` and `memory list` still work when you are
inside a git repository: with no `.spelunk/` project, `add` rides the same
write-through carrier (there is no SQLite primary yet) and `list` reads entries
back from `refs/notes/spelunk`. Because it is the same write path pre- and
post-`init`, every note carries an identical record shape. `memory search` and
`context` remain gated to projects with `.spelunk/` (they need the index to
search and embed).

**Store priority** (unchanged from [ADR-004](adr/004-unified-memory-storage.md)):

1. Explicit `--db <path>` (always wins)
2. Explicit `--backend git-notes` (git notes is the primary store)
3. Explicit team `server_url` in config (remote server)
4. A local `.spelunk/memory.db` (after `spelunk init`)
5. No project but inside a git repo: the git-notes write-through carrier (add/list only)
6. Neither a project nor a git repo: error, *"no spelunk project here, and not inside a git repo. Run 'spelunk init' first, or run inside a git repository."*

**Known limitation:** git-notes writes are not atomic across concurrent `add`
commands to the same commit (the read-modify-write can lose an entry if two
agents write simultaneously). This is acceptable for the solo, pre-`init`
quick-fix case; multi-agent workflows should `spelunk init` and use SQLite. Note
also that notes under `refs/notes/spelunk` are **not** pushed or fetched by
default, so pre-`init` entries stay on the machine that wrote them until the
notes ref is pushed (see [Sharing memory across clones via
git-notes](#sharing-memory-across-clones-via-git-notes) below, and the
[git notes](https://git-scm.com/docs/git-notes) documentation).

See [ADR-067](adr/067-fail-closed-no-local-project.md) for the fail-closed design
and [ADR-068](adr/068-zero-setup-onboarding-git-notes-memory-fallback.md) for the
git-notes carrier rationale.

### Sharing memory across clones via git-notes

Reading and publishing are not symmetric, and it is worth being precise about
which is automatic:

- **Reading teammates' memory is automatic.** `spelunk init` configures the
  `origin` fetch refspec, so their notes arrive on your next `git fetch`, and
  spelunk merges them on its own read paths.
- **Publishing your own memory is opt-in.** Your memory stays local until you
  install the pre-push hook (or push the notes ref by hand).

When you run `spelunk init` inside a git repository with an `origin` remote,
spelunk automatically configures the fetch refspec for `origin` so that
teammates' `refs/notes/spelunk` travels on `git fetch`. The init command prints
the status:

```
Memory:  configured notes fetch refspec on 'origin' (teammates' memory arrives on fetch)
         your memory stays local until you install the pre-push hook: spelunk hooks install --pre-push
         configured notes.rewriteRef (memory survives `git commit --amend` and `git rebase`)
```

The last line is a separate setting, covered in [Surviving history
rewrites](#surviving-history-rewrites) below. It is printed only by the run that
sets it, so a re-run of `init` omits it.

#### Publishing with the pre-push hook

Install the hook once per clone:

```bash
spelunk hooks install --pre-push
```

From then on, every `git push` publishes your memory to the remote you are
pushing to: the hook fetches the remote's notes, merges them into yours (a
union, so nothing is dropped), and pushes `refs/notes/spelunk`. Once it is
installed, the second line of `init`'s summary changes to confirm it.

**Publishing is tied to `git push` on purpose.** A note attached to a commit you
have not pushed can reach the remote while the commit itself does not, and a
teammate's clone then cannot resolve what the note is attached to, so the entry
is orphaned: it is on the remote, and nobody ever sees it. Pushing is the moment
that reliably coincides with "this code is being shared", which is why the hook
runs there rather than on each `memory add` or on a timer.

**The hook never blocks your push.** If publishing fails (offline, or the remote
rejects the notes ref) it warns on stderr and exits 0, so your code push lands
regardless. It retries a lost race up to three times, and never force-pushes: the
union already carries both sides, so forcing could only discard a teammate's
memory.

Teammates without spelunk installed are unaffected: the hook skips itself when
`spelunk` is not on their PATH. Remove it with `spelunk hooks uninstall`.

#### Publishing without the hook

If you would rather not install a hook, push the notes ref yourself:

```bash
git push origin refs/notes/spelunk
```

Re-run this whenever you record memory: each `spelunk memory add` (or remove)
creates a new notes commit that travels only once it is pushed. Push it **after**
you have pushed the commits your entries are attached to, or those entries arrive
orphaned (see above). The hook exists to get that ordering right for you.

The fetch refspec, by contrast, is configured once, so teammates' (and later
clones') `git fetch` then pulls whatever notes you have already pushed.

**How fetched notes become visible.** The refspec fetches into a *tracking* ref,
`refs/notes/origin/spelunk`, rather than over your own `refs/notes/spelunk`.
Fetching straight onto your working ref would force-update it and silently
replace a local note you had not pushed yet. So arrival is **fetch + merge**:
`git fetch` populates the tracking ref, and `spelunk memory list`, `spelunk
context`, and `spelunk init` merge it into `refs/notes/spelunk` (union, no
conflicts, duplicates dropped). That merge is local-only and does no network: it
folds in what your own `git fetch` already brought down, so it works with the
remote unreachable, and it never picks up remote state on its own. Right after a
fetch, `git notes --ref=spelunk` alone will not show a teammate's entry until one
of those spelunk commands has run.

The merge never delays or fails a read. If another spelunk command is writing
notes at that moment, the merge is skipped and the read returns anyway; the union
is idempotent, so the next read folds the entries in.

**For teammates to receive the notes:**

1. Clone the repository normally: `git clone <repo>`
2. Run `spelunk init` in the clone (or manually add the refspec with `git config --add remote.origin.fetch '+refs/notes/spelunk*:refs/notes/origin/spelunk*'`)
3. Fetch: `git fetch`
4. Read: `spelunk memory list` (this is the step that merges the fetched notes in)

A fresh clone does **not** inherit the source's local git config, so `git fetch`
alone won't pull the notes. The teammate must either run `spelunk init` (which
configures the refspec automatically) or add it manually, then fetch.

**If there is no `origin` remote** (for example, in a local-only or detached
repository), `spelunk init` prints the commands to run later:

```
Memory:  no 'origin' remote, so the notes refspec is not configured
         run later: git config --add remote.origin.fetch '+refs/notes/spelunk*:refs/notes/origin/spelunk*'
         your memory stays local until you install the pre-push hook: spelunk hooks install --pre-push
         configured notes.rewriteRef (memory survives `git commit --amend` and `git rebase`)
```

Add the refspec when an `origin` is created, then publish as above. The
`notes.rewriteRef` line appears here too: rewrites are purely local, so that
setting is configured even in a repository with no remote.

If the repository already carries memory on `refs/notes/spelunk` (for example a
fresh clone of a project whose team records memory through git notes), `spelunk
init` **hydrates** the new `memory.db` from those notes: every entry not already
present is imported, and `spelunk memory list` then shows the repo's recorded
history. The import is idempotent (re-running `init` imports nothing) and copies
entry content only, not embeddings, so imported entries appear in `memory list`
and full-text search right away. This is a local import: the notes must already
be present in your clone. Their cross-machine arrival still depends on your git
notes refspec, since git does not fetch `refs/notes/*` by default (see above).
git-notes is the durable carrier here and `memory.db` is a local index rebuilt
from it; see
[ADR-068](adr/068-zero-setup-onboarding-git-notes-memory-fallback.md).

### Surviving history rewrites

`git commit --amend` and `git rebase` replace a commit with a new sha. A note is
bound to the sha it was written on, and git carries it onto the replacement only
when `notes.rewriteRef` names the ref the note lives on. That setting has **no
built-in default**: in an unconfigured repository, amending or rebasing a commit
that carries memory orphans every entry on the dead sha. `memory list` never
shows those entries again, because it lists notes that are reachable from `git
log`, and the dead sha no longer is.

spelunk therefore points `notes.rewriteRef` at `refs/notes/spelunk` for you. It
is written to the repository's own config, never your global config, at whichever
of these comes first:

- `spelunk init`, alongside the fetch refspec. Independent of `origin`, since
  rewrites are purely local, so a repository with no remote is covered too.
- The first `memory add` write-through, which reaches repositories where you
  never run `init`.
- The `--backend git-notes` write path, where notes are the primary store.

Setting it is announced, never silent. The run that sets it prints:

```
Configured git notes.rewriteRef in this repo, so memory now survives `git commit --amend` and `git rebase`.
```

Later runs stay quiet, since the setting is already in place. `--add` composes
with any value you set yourself rather than replacing it, and an existing value
that already covers the ref (exactly, or via a glob that stays inside
`refs/notes/`) is left alone. If the setting cannot be written, spelunk warns and
continues rather than failing the write, and names the command to run:

```
Warning: could not set git notes.rewriteRef, so memory may not survive `git commit --amend` or `git rebase`. Set it with: git config --add notes.rewriteRef refs/notes/spelunk
```

`notes.rewriteMode` is deliberately left at its `concatenate` default, which
keeps both sides when two noted commits are squashed into one. `overwrite` and
`ignore` each drop one of them, causing the loss this is meant to prevent.

**Known limitation:** git honours `notes.rewriteRef` for `commit --amend` and
`rebase` only. `git merge --squash`, and cherry-picking onto a divergent base, do
**not** carry notes, even with the setting configured. Memory attached to a
commit that reaches your branch by either of those routes is still orphaned on
the original sha. If those entries matter, re-record them on the new commit
before the original is discarded.

This is about surviving a rewrite of your own local history. It is a separate
concern from whether notes reach a teammate, which still depends on the notes ref
being pushed and fetched (see above).

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

### Web-to-Markdown hook (opt-in) {#web-to-md-hook}

For non-GitHub URLs, if a script exists at `~/.config/spelunk/scripts/web-to-md.ts`, `spelunk` runs it under `bun` (`bun ~/.config/spelunk/scripts/web-to-md.ts <url>`) and uses its stdout instead of the built-in HTML-stripping fallback — useful for sites that need JS rendering or custom extraction logic. The script's first line (`# Title`) becomes the entry title; the rest becomes the body.

This is opt-in by design: the script only runs if you've placed it at that exact, spelunk-owned path. Requires [`bun`](https://bun.sh) on `PATH`. If `bun` or the script fails, `spelunk` silently falls back to the built-in HTML extraction. Set `SPELUNK_SCRIPTS_DIR` to look for the script in a different directory instead of `~/.config/spelunk/scripts`.

> **Breaking change:** prior to this, `spelunk` looked for the hook script at
> `~/scripts/web-to-md.ts`. That location is **no longer read** — any script
> left there is silently ignored, and `memory add --from-url` falls back to
> the built-in HTML extraction instead. If you were relying on the old hook,
> move the script to `~/.config/spelunk/scripts/web-to-md.ts` (creating the
> directory if needed). The path moved because the old, undocumented
> `~/scripts/` convention meant *any* script an attacker could plant there —
> via an unrelated prior compromise, or on a shared/managed machine — would
> get silently executed on every `--from-url` call; the new path is scoped to
> a location you explicitly manage for spelunk.

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

`--branch` and `--git-range` values (or either endpoint of an `A..B` range) that start with `-` are rejected before reaching `git`, with a clear error — this prevents a malformed or attacker-controlled ref from being parsed as a `git log` option.

### Automatic harvesting

Install the git hook and harvesting happens on every commit:

```bash
spelunk hooks install
```

To also publish memory to your remote on every `git push`, install the pre-push
hook (see [Sharing memory across
clones](#sharing-memory-across-clones-via-git-notes)):

```bash
spelunk hooks install --pre-push
```

## Importing from a local server

`spelunk memory reconcile` imports notes that were recorded by a running
`spelunk-server` daemon into the project's local `memory.db`. This is useful
after a session where entries were written through `server_url` and need to be
pulled into the project's local store, or when migrating from server-backed to
local storage.

Dedup is by content identity (see **Entry identity** at the top of this page): a
note whose `kind`, `title`, and `body` already match an entry in `memory.db` is
skipped. The source `server.db` is opened read-only; it is never modified.

Because that identity covers only those three fields, source rows that differ
*only* in creation time, tags, or linked files are the same entry, and collapse
into a single row on import. Tags and linked files are merged onto the survivor,
adding but never removing, so nothing recorded on a collapsed copy is lost.
`--format json` reports the count as `collapsed_duplicates`, and the counts
partition the source rows exactly:
`candidates == already_present + collapsed_duplicates + imported`.

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
