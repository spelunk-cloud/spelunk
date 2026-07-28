# CLI Capability Tiers

**Issue:** #259  
**Status:** Implemented (v0.8.0)

---

## Overview

The spelunk CLI operates in one of two capability tiers determined at runtime
by whether a `spelunk-server` is reachable. No compile-time feature flags are
used; the binary is the same in both tiers.

| | Tier 0 — Offline | Tier 1 — Server-connected |
|---|---|---|
| **Condition** | No `server_url` configured, or server unreachable | `server_url` set and health probe succeeds |
| **Search** | ast-grep + BM25 text | + semantic KNN (server encodes query, CLI does local KNN) |
| **Index** | Parse + AST chunk + graph (no embeddings) | + embedding phase: server generates vectors, CLI stores in local DB |
| **Memory add/list/show/archive** | sqlite (local) | Same |
| **Memory push/pull** | Not available | Sync git-notes entries to/from server DB |
| **Memory search** | Not available | Server encodes query, does KNN over server-side memory DB |
| **Memory harvest** | Not available | LLM extraction via server |
| **Explore** | Not available | CLI pre-fetches context chunks locally, sends to server LLM loop |

**The CLI never calls embedding or LLM APIs directly, regardless of
configuration.** All inference routes through `spelunk-server`.

> **Reserved: Plan.** `/plan` is reserved as a server-owned route per ADR-002,
> but nothing ships today: there is no `spelunk plan` subcommand and no `/plan`
> server route. The CLI parses a `plan` capability from the server health
> response but deliberately keeps it out of all output, so it never surfaces.

**Tier 0 requires no external tools.** Uses `ast-grep` structural search.

---

## Configuration

### New unified field: `server_url`

Add `server_url` to `Config` as the single entry point for all server-mediated
features.

```toml
# ~/.config/spelunk/config.toml  (personal — never commit)
server_url = "https://spelunk.internal.example.com"
server_key = "sk-..."

# .spelunk/config.toml  (project-level — safe to commit if key is in env)
project_id = "acme/my-app"
server_url = "https://spelunk.internal.example.com"   # key via SPELUNK_SERVER_KEY env var
```

> `server_url` must be `https://` unless it resolves to loopback
> (`127.0.0.1` / `::1` / `localhost`); a non-loopback `http://` value is
> rejected at config-load time with no override, since the bearer token is
> attached to every server-mediated request. See
> [Server setup → Trust model](../server-setup.md#trust-model).

Environment variable overrides:

| Field | Env var |
|---|---|
| `server_url` | `SPELUNK_SERVER_URL` |
| `server_key` | `SPELUNK_SERVER_KEY` |
| `project_id` | `SPELUNK_PROJECT_ID` |

### Validation

`server_url` present without `project_id` → hard error at load time.

---

## Capability probe

The probe runs **lazily** — not at CLI startup, but on the first command that
needs Tier 1. Once the result is known it is cached for the process lifetime
(no repeated probes).

Algorithm:

```
fn probe_server(cfg: &Config) -> Tier {
    let Some(url) = cfg.server_url else { return Tier::Offline };
    match GET {url}/v1/health within 2s timeout {
        Ok(200, body) => {
            let caps = body["capabilities"].as_array();
            Tier::Server { capabilities: caps }
        }
        _ => {
            warn!("spelunk-server at {url} unreachable — running in offline mode");
            Tier::Offline
        }
    }
}
```

The `capabilities` field in the health response (see server-api.md) allows the
CLI to degrade gracefully if an older server version is deployed that lacks
newer endpoints.

---

## Loopback auto-discovery

**Issue:** #303

In v0.8.0 the common case is no `server_url` at all: the CLI discovers (or
starts) a **local** server on the loopback address. This is what makes Tier 1
the default for a fresh single-user install — semantic search and `explore`
work out of the box without the user configuring or managing a server.

Discovery runs before the configured-`server_url` probe and only on loopback:

```
fn discover_local_server() -> Option<ServerHandle> {
    if env::var("SPELUNK_NO_SERVER").is_ok() { return None; }   // hard opt-out

    // 1. Probe the well-known loopback endpoint.
    match GET http://127.0.0.1:7777/v1/health within 250ms {
        Ok(200, body) => {
            // 2. Only reuse a server this user owns.
            if body["started_by"] == current_uid() {
                return Some(ServerHandle::existing(body["instance_id"]));
            }
            // Owned by another UID — do not reuse; fall through to no-server.
            warn!("server on 127.0.0.1:7777 started by another user — not reusing");
            return None;
        }
        _ => {}
    }

    // 3. Nothing reachable — autostart the bundled server in the background.
    Some(ServerHandle::spawn_bundled())
}
```

Key points:

- **Address.** Discovery is fixed to `127.0.0.1:7777` — loopback only, never a
  routable interface. A team/remote server is reached through explicit
  `server_url` config, not discovery.
- **`instance_id`.** Each running server reports a unique UUID v7 in its
  `/v1/health` body. The CLI logs it at debug level and uses it to detect
  a server that was restarted underneath a session. Implemented in both server
  and CLI (shipped with PRs #329/#333).
- **`started_by` (UID check).** The health body includes the effective UID of
  the process that started the server. The CLI warns (but does not block) when
  the server was started by a different user — a security hint on shared
  machines. Implemented in both server and CLI (shipped with PRs #329/#333).
- **Autostart.** If nothing is reachable, the CLI spawns the bundled
  `spelunk-server` as a background child owned by the current user, then waits
  for its health endpoint before proceeding.
- **`SPELUNK_NO_SERVER`.** When set, discovery is skipped entirely: no probe, no
  autostart. The CLI runs in Tier 0 and inference-only commands exit 1 with the
  locked-feature message.

<!-- The discovery timeout (250 ms) and autostart/handshake UX are confirmed
     against capability/probe.rs. `instance_id` and `started_by` are implemented
     (PRs #329/#333). -->

User-facing behaviour for these tiers is documented in
[getting-started.md → Capability tiers](../getting-started.md#capability-tiers-where-inference-and-memory-live).

---

## spelunk status — capability section

`spelunk status` gains a capability section above the index stats.

**Text output (Tier 0 — offline):**

```
Capability tier:  Offline
  search          ast-grep + text  [set server_url to enable semantic search]
  memory          sqlite (local)
  explore         unavailable  [set server_url to enable]
```

The `memory` line reflects the resolved backend (`sqlite` / `git-notes` /
`remote`), not the capability tier. In a directory with no local `.spelunk/`
project, `spelunk status` reports `No spelunk project here` instead (see
[fail-closed, ADR-067](../adr/067-fail-closed-no-local-project.md)).

**Text output (Tier 1 — server connected):**

```
Capability tier:  Server  (https://spelunk.internal.example.com)
  search          ast-grep + text + semantic
  embedder        ready
  memory          sqlite (local)
  explore         available
```

The `embedder` line reports the server's `embedder.state` from `/v1/health`; it
is omitted when the server does not report that field.

**JSON output** (`spelunk status --format json`) adds a `capabilities` object
(other fields omitted):

```json
{
  "tier": "server",
  "server_url": "https://spelunk.internal.example.com",
  "capabilities": {
    "explore": true,
    "index_embed": true,
    "memory_harvest": true,
    "memory_pull": true,
    "memory_push": true,
    "memory_search": true,
    "search_semantic": true
  }
}
```

---

## spelunk check — server probe addition

`spelunk check` (text mode only) appends a server status line when
`server_url` is configured:

```
Index is up to date. (412 files indexed)
Server:  https://spelunk.internal.example.com  ✓  (semantic search, explore available)
```

Or on failure:

```
Index is up to date. (412 files indexed)
Server:  https://spelunk.internal.example.com  ✗  unreachable — offline mode
```

---

## Error messages for locked features

When a Tier 1 feature is invoked but no server is reachable, the command exits
1. Two deliberate message formats are used, selected by which command was run;
both are written to stderr with `eprintln!` (never a panic).

The `require_tier1` commands (`explore`, `memory push`, `memory pull`, `sync`,
`memory watch`) point the user at `server_url`:

```
Error: 'spelunk explore' requires spelunk-server.
Set server_url in ~/.config/spelunk/config.toml to enable this feature.
       (Tried: https://spelunk.internal.example.com — connection refused)
```

The `(Tried: ...)` line is appended only when a `server_url` is configured but
unreachable. If `server_url` is not set at all it is omitted:

```
Error: 'spelunk explore' requires spelunk-server.
Set server_url in ~/.config/spelunk/config.toml to enable this feature.
```

The inference-only commands (`memory search`, `memory harvest`) point the user
at the local server instead, and also exit 1:

```
Error: 'spelunk memory search' requires spelunk-server.
Run `spelunk server start` to enable this feature.
```

---

## spelunk index — two-phase behaviour

### Phase 1 (always, Tier 0 and Tier 1)

Parse files → produce chunks → extract AST graph edges → store in local DB.
No embeddings generated. Existing behaviour for text/ast-grep search is fully
preserved.

### Phase 2 (Tier 1 only)

After Phase 1 completes, if a server is reachable:

1. Collect all chunks that lack an embedding in the local DB.
2. Batch-send to `POST /v1/projects/{id}/index/embed` (see server-api.md).
3. Server returns vectors for each chunk.
4. CLI writes vectors into local DB.
5. Server discards the vectors — it is a processing endpoint, not storage.

The two phases are independent. A partial Phase 2 (network failure mid-batch)
is safe: chunks without embeddings remain in the DB and will be embedded on
the next `spelunk index` run. Phase 1 is never re-run for unchanged files
(blake3 hash check is unaffected).

Phase 1 itself is also crash-safe. The content-hash write and the chunk
writes are not spanned by one transaction, so a kill between them can leave a
file recorded as hash-current with zero chunks. `Database::file_has_chunks`
(`storage/files.rs`) makes the skip check require actual stored chunks, not
just a matching hash, so the next plain run detects that half-indexed state
and reprocesses the file instead of skipping it forever.

The whole `spelunk index` process, both phases, is serialized per project by
a cross-process advisory lock (`cli/cmd/index/run_lock.rs`), taken as the
first thing a run does and released on process exit. Two concurrent runs
against the same project previously could interleave writes and corrupt
`index.db`; a second run that finds the lock held now exits immediately with
a clean error instead of racing the first run's writes.

Progress output during Phase 2:

```
Embedding chunks via server... 1 024 / 3 812  [====>     ] 27%
```

---

## Memory search (Tier 1 only)

`spelunk memory search "<query>"` sends the query text to the server. The
server encodes the text and runs KNN over its memory DB. The raw-vector
interface (`SearchRequest.embedding`) is deprecated; see server-api.md for the
updated `SearchRequest` schema.

---

## Explore — context assembly

For `spelunk explore`, the CLI is responsible for context
retrieval from the local index before calling the server. This preserves data
ownership: chunk content is never pushed to the server for storage.

Flow:

1. CLI runs local text + (if available) semantic search to assemble
   `context_chunks`.
2. CLI sends `{query, context_chunks}` to `POST /v1/projects/{id}/explore`
   (SSE).
3. Server runs LLM reasoning loop over the provided context.
4. Server does not store the context chunks.

`context_chunks` are ephemeral — they exist only for the duration of the
request.
