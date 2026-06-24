# AGENT.md — spelunk-powered project

> **Template for end users of spelunk.**
> Copy this file to the root of your project (rename to `AGENT.md` or `CLAUDE.md`),
> adjust the project-specific sections, and commit it. This instructs AI agents
> to use spelunk for context retrieval rather than brute-force file reads.

---

## Context retrieval — use spelunk

This project uses [spelunk](https://github.com/spelunk-cloud/spelunk) for code graph traversal, memory, and search.

```bash
# Trace a symbol's callers and callees (no server needed)
spelunk graph verify_token

# Full-text search (no server needed)
spelunk search "error handling" --mode text

# Semantic search — finds code by meaning (requires embedding server + index)
spelunk search "how does authentication work"

# Ask a natural language question (requires llm_model in config)
spelunk ask "what does the retry logic do when the upstream times out?"
```

**Rule:** run `spelunk graph <symbol>` and `spelunk search "<topic>" --mode text` before opening files you haven't read this session. Fall back to `Read`/`Grep`/`Glob` when these return nothing useful.

---

## Recorded decisions and context

Past architectural decisions, requirements, and open questions are stored in
spelunk memory. Check them at the start of every session:

```bash
spelunk memory list --kind decision --limit 10   # prior design decisions
spelunk memory list --kind handoff --limit 3     # where last session left off
spelunk memory list --kind question              # open questions
spelunk memory search "topic you care about"    # semantic search over memory
```

Store new decisions as you make them — don't wait until the end:

```bash
spelunk memory add --kind decision \
  --title "Why we use X instead of Y" \
  --body "reason, alternatives considered, what breaks if changed"

spelunk memory add --kind requirement \
  --title "Must support offline mode" \
  --body "user stated this as hard requirement on 2026-04-01"
```

---

## Plumbing commands (for scripting and pipelines)

spelunk exposes machine-readable plumbing commands for use in scripts:

```bash
# Stream all indexed chunks for a file as JSONL
spelunk plumbing cat-chunks src/auth.rs

# Parse a file and emit AST chunks without writing to the DB
spelunk plumbing parse-file src/auth.rs

# Embed text and pipe into vector search
echo "token refresh flow" | spelunk plumbing embed --query \
  | spelunk plumbing knn --limit 5

# Check if a file has changed since last index
spelunk plumbing hash-file src/auth.rs

# Stream raw graph edges for a symbol
spelunk plumbing graph-edges --symbol verify_token
```

All plumbing commands emit JSONL. Exit 0 = results, 1 = no results, 2 = error.

---

## Re-indexing (if project uses semantic search)

spelunk indexes are incremental. Re-run after significant changes:

```bash
spelunk index .            # index the current directory
spelunk check              # verify the index is fresh
```

A post-commit hook can do this automatically — see `spelunk hooks install`.

---

## Project-specific notes

<!-- Customise this section for your project -->

**Tech stack:** <!-- e.g. Rust, PostgreSQL, React -->  
**Key entry points:** <!-- e.g. src/main.rs, src/api/routes.rs -->  
**Test command:** <!-- e.g. cargo test / pytest / npm test -->  
**Build command:** <!-- e.g. cargo build --release -->  

---

## What spelunk cannot do

- It cannot run your tests or build the project — use shell commands for that
- Semantic search results are only as fresh as the last `spelunk index` run
- `spelunk ask` and `spelunk explore` require `llm_model` in `~/.config/spelunk/config.toml`
- `spelunk search` (semantic) requires an embedding server and a built index; use `--mode text` for full-text search without either
