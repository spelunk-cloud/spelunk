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
| **Memory add/list/show/archive** | git-notes (local) | Same |
| **Memory push/pull** | Not available | Sync git-notes entries to/from server DB |
| **Memory search** | Not available | Server encodes query, does KNN over server-side memory DB |
| **Memory harvest** | Not available | LLM extraction via server |
| **Explore** | Not available | CLI pre-fetches context chunks locally, sends to server LLM loop |
| **Plan** | Not available | LLM planning via server |

**The CLI never calls embedding or LLM APIs directly, regardless of
configuration.** All inference routes through `spelunk-server`.

---

## Configuration

### New unified field: `server_url`

Add `server_url` to `Config` as the single entry point for all server-mediated
features. The existing `memory_server_url` field is a backward-compat alias:
if `server_url` is absent and `memory_server_url` is present, treat
`memory_server_url` as the `server_url` value. Log a deprecation warning
once.

```toml
# ~/.config/spelunk/config.toml  (personal — never commit)
server_url = "http://spelunk.internal:7777"
server_key = "sk-..."

# .spelunk/config.toml  (project-level — safe to commit if key is in env)
project_id = "acme/my-app"
server_url = "http://spelunk.internal:7777"   # key via SPELUNK_SERVER_KEY env var
```

Environment variable overrides:

| Field | Env var |
|---|---|
| `server_url` | `SPELUNK_SERVER_URL` (also `SPELUNK_MEMORY_SERVER_URL` as alias) |
| `server_key` | `SPELUNK_SERVER_KEY` |
| `project_id` | `SPELUNK_PROJECT_ID` |

### Validation

`server_url` present without `project_id` → hard error at load time (same as
current `memory_server_url` validation).

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
the default for a fresh single-user install — semantic search, `explore`, and
`plan` work out of the box without the user configuring or managing a server.

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
  autostart. The CLI runs in Tier 0 and inference-only commands exit 2 with the
  locked-feature message.

<!-- The discovery timeout (250 ms) and autostart/handshake UX are confirmed
     against capability.rs. `instance_id` and `started_by` are implemented
     (PRs #329/#333). -->

User-facing behaviour for these tiers is documented in
[getting-started.md → Server mode vs no-server mode](../getting-started.md#server-mode-vs-no-server-mode)
and [server.md → Local server](../server.md#local-server-automatic--no-setup).

---

## spelunk status — capability section

`spelunk status` gains a capability section above the index stats.

**Text output (Tier 0 — offline):**

```
Capability tier:  Offline
  search          ast-grep + text
  memory          git-notes (local)
  explore         unavailable  [set server_url to enable]
  plan            unavailable  [set server_url to enable]
```

**Text output (Tier 1 — server connected):**

```
Capability tier:  Server  (http://spelunk.internal:7777)
  search          ast-grep + text + semantic
  memory          git-notes + server sync
  explore         available
  plan            available
```

**JSON output** (`spelunk status --format json`) adds a `capabilities` object:

```json
{
  "tier": "server",
  "server_url": "http://spelunk.internal:7777",
  "capabilities": {
    "search_semantic": true,
    "index_embed": true,
    "memory_push": true,
    "memory_pull": true,
    "memory_search": true,
    "memory_harvest": true,
    "explore": true,
    "plan": true
  }
}
```

---

## spelunk check — server probe addition

`spelunk check` (text mode only) appends a server status line when
`server_url` is configured:

```
Index is up to date. (412 files indexed)
Server:  http://spelunk.internal:7777  ✓  (semantic search, explore, plan available)
```

Or on failure:

```
Index is up to date. (412 files indexed)
Server:  http://spelunk.internal:7777  ✗  unreachable — offline mode
```

---

## Error messages for locked features

When a Tier 1 feature is invoked but no server is reachable, exit 2 with a
consistent message format:

```
error: 'spelunk explore' requires spelunk-server.
       Set server_url in ~/.config/spelunk/config.toml to enable this feature.
       (Tried: http://spelunk.internal:7777 — connection refused)
```

If `server_url` is not set at all:

```
error: 'spelunk explore' requires spelunk-server.
       Set server_url in ~/.config/spelunk/config.toml to enable this feature.
```

Use `eprintln!` to stderr. Do not panic.

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

## Explore and Plan — context assembly

For `spelunk explore` and `spelunk plan`, the CLI is responsible for context
retrieval from the local index before calling the server. This preserves data
ownership: chunk content is never pushed to the server for storage.

Flow:

1. CLI runs local text + (if available) semantic search to assemble
   `context_chunks`.
2. CLI sends `{query, context_chunks}` to `POST /v1/projects/{id}/explore`
   (SSE) or `POST /v1/projects/{id}/plan`.
3. Server runs LLM reasoning loop over the provided context.
4. Server does not store the context chunks.

`context_chunks` are ephemeral — they exist only for the duration of the
request.

---

## Definition of done

- [ ] `Config` gains `server_url` / `server_key` fields; `memory_server_url`
  aliased with deprecation warning
- [ ] Capability probe implemented and cached per-process
- [ ] `spelunk status` shows capability tier section
- [ ] `spelunk check` shows server reachability line when `server_url` is set
- [ ] Error messages follow the format above for all locked features
- [ ] `spelunk index` Phase 2 implemented (embedding via server)
- [ ] All existing `cargo test` suites pass; `cargo fmt` + `cargo clippy` clean
