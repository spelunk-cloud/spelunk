---
name: spelunk
description: Use the spelunk CLI to retrieve code context and persist project memory. Search code semantically or by text, trace the call graph, and store decisions, requirements, questions and handoffs across sessions. Use at the start of any session in a spelunk-indexed repo, before reading or editing unfamiliar code, and whenever a decision or handoff should outlive the session.
---

# spelunk

**The mental model: spelunk retrieves context; you reason over it.** Use `graph` and `search` to
find the right code, read the results, then synthesise the answer yourself. It is a persistent
memory store and code-navigation tool, not an oracle.

## The core loop

1. **Orient**: read memory, check index health
2. **Search**: find relevant code *before* reading or editing it
3. **Execute**: make the change
4. **Verify**: re-check the call graph, re-index
5. **Codify**: store decisions and a handoff

Each session leaves better context for the next, whether that is you resuming or a different agent
picking up.

## Start every session

```bash
spelunk context   # handoffs, open questions, decisions, requirements from prior sessions
spelunk check     # is the index fresh? (only if the project is indexed)
```

`spelunk context` is the single agent entry point. It returns the four most agent-relevant memory
sections newest-first, plus heuristically extracted coding conventions.

Useful flags: `--budget N` (cap total output tokens, keeping durable decisions and requirements
ahead of open questions under pressure), `--kind decision`, `--path src/auth`, `--limit N`,
`--format json`.

## Search before you write

Never open a file cold when a search would find it faster:

```bash
spelunk graph <symbol>                 # callers/callees; works live, no index or server needed
spelunk search "<topic>" --mode text   # full-text; always works
spelunk search "<topic>"               # semantic; needs index + server
spelunk chunks <file>                  # what was actually indexed for a file
```

After changing code, re-check call sites with `spelunk graph <symbol>` and re-index with
`spelunk index .`. Indexing is incremental and blake3-gated, so it is cheap.

## Store memory as you go

**Do not batch this to the end of the session.** A decision recorded when you make it captures the
alternatives you rejected; one reconstructed an hour later usually does not.

```bash
spelunk memory add --kind decision    --title "..." --body "why, what alternatives, what breaks"
spelunk memory add --kind requirement --title "..." --body "..."   # a constraint someone stated
spelunk memory add --kind note        --title "..." --body "..."   # surprising, non-obvious fact
spelunk memory add --kind question    --title "..." --body "..."   # needs a human, resolved async
spelunk memory add --kind handoff     --title "Handoff: ..." --body "done / next / open questions"
```

Query it with `spelunk memory list --kind decision --limit 10`, `spelunk memory search "<topic>"`,
`spelunk memory show <id>`, `spelunk memory timeline`.

**End every substantial session with a `--kind handoff` entry.** It is what the next session reads
first.

## Machine-readable output

```bash
export AGENT=true      # every command returns JSON
```

Or `--format json` per command. Use this when you are parsing output rather than reading it.

## Where memory lives

Always the project's local `memory.db`. A loopback `spelunk-server` is an **inference backend
only**: it embeds queries and runs LLM calls, and never stores memory. Memory moves off the local
DB only when a team `server_url` is *explicitly* configured.

For `memory search`, only the query text is sent to the local embedder; the vector search runs
locally against `memory.db`. Note text never leaves the local store.

## The server

Semantic search, `spelunk explore` and `spelunk memory harvest` need `spelunk-server` for
inference. From v0.8.0 it autostarts locally on demand and bundles a native embedder
(F2LLM-v2-330M, 896-dim, GPU-accelerated on macOS), so there is no external embedding endpoint.

```bash
spelunk server start|status|logs|stop   # start is idempotent, safe every session
```

With `SPELUNK_NO_SERVER=1` those commands fall back to text/ast-grep search or error clearly.
Everything else (memory, full-text and ast-grep search, code graph, conventions) needs only the
CLI binary.

## Working *on* the spelunk repo itself

Two rules that will otherwise cost you a session:

- **`SPELUNK_SECRET_STORE=file` on every single cargo command**, without exception, as in
  `SPELUNK_SECRET_STORE=file cargo test -p spelunk-cli`. A bare cargo command reaches the real OS
  keychain and blocks on an interactive permission dialog that never resolves on its own. If a
  cargo command hangs past ~20s with no output, kill it and check this first.
- **It is a public repo.** Never write an internal tracker or board reference into shipped code,
  comments, test names, commit messages or docs. Describe what changed and why, never which ticket.

Full reference: `SKILL.md` and `docs/agent-guide.md` in the spelunk repo.
