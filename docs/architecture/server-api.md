# spelunk-server HTTP API Contract

**Issue:** #261  
**Status:** Implemented. This is the current reference for the HTTP + SSE
surface `spelunk-server` exposes. Sections below describe what the running
server actually does, verified against `crates/spelunk-server/src/handlers.rs`.

---

## Overview

This document specifies the HTTP API surface that `spelunk-cli` calls on
`spelunk-server`: the `AuthProvider` trait, and every route the server exposes.

1. The `AuthProvider` trait, which every route the server exposes goes through.
2. Endpoints present from the server's first API-key auth implementation.
3. Endpoints added for CLI integration (embedding proxy, explore, LLM completion).
4. The server's **data promise** to the CLI.

---

## Data promise (server → CLI)

The CLI is the only durable store for index data. The server's behaviour is
constrained as follows.

| Resource | Server may receive | Server may store | Server may cache (in-memory, bounded TTL) |
|---|:---:|:---:|:---:|
| Chunk content (text) for embedding | yes | **no** | yes (eviction within session) |
| Embedding vectors generated for the CLI | n/a (server emits) | **no** | yes (request-scoped) |
| Search queries (text) | yes | **no** (logs may carry metadata only — never the query body in plaintext) | yes (LRU for rate limiting) |
| `context_chunks` sent with `/explore` | yes | **no** | request-scoped only |
| Memory entries (notes) | yes | **yes** (this IS the server's persistence role) | n/a |
| Project metadata (id, slug, stats) | yes | yes | n/a |
| Auth principals (api keys, user ids) | yes | hash/identifier only — never the raw bearer | yes |

**Rationale.** The server is an inference + memory peer, not a code-index
mirror. The CLI fingerprints every chunk locally (blake3) and writes vectors
to its own sqlite-vec store; the server has no business duplicating that.
Persisting embeddings server-side would also force the OSS deployment into a
data-residency conversation we do not want before cloud.

This promise is testable: server integration tests assert that the embedding
DB table is empty after `/v1/projects/{id}/index/embed` returns, and that
`/explore` writes nothing to memory.

---

## Auth architecture

Requests are authenticated through a pluggable `AuthProvider` trait
(`crates/spelunk-server/src/auth.rs`), so an alternative auth strategy can be
added without changing any handler. The shipped implementation, `ApiKeyAuth`,
checks the `Authorization: Bearer` header against a single configured key
(`SPELUNK_SERVER_KEY`); with no key configured, every request is accepted,
which is safe only when the server is bound to loopback (see [Trust
model](../server-setup.md#trust-model)). No handler currently reads the
authenticated principal.

---

## Error response format

All error responses use a consistent JSON body:

```json
{ "error": { "code": "unauthorized", "message": "Bearer token required" } }
```

| HTTP status | `code` |
|---|---|
| 400 | `bad_request` |
| 401 | `unauthorized` |
| 404 | `not_found` |
| 409 | `conflict` (memory only — entry stored, conflicts returned) |
| 500 | `internal_error` |

---

## Existing endpoints

### `GET /v1/health`

Returns `200` JSON (unauthenticated: no bearer token required or attached):

```json
{
  "status": "ok",
  "version": "0.9.4",
  "instance_id": "550e8400-e29b-41d4-a716-446655440000",
  "started_by": 501,
  "embedding_dim": 896,
  "embedder": { "state": "ready" },
  "capabilities": [
    "memory",
    "index.embed",
    "search.semantic",
    "explore",
    "llm.complete"
  ],
  "limits": {
    "embed_request_timeout_secs": 1800,
    "max_batch_chunks": 256,
    "embedder_token_cap": null
  }
}
```

The `capabilities` array lists features this server instance supports. The CLI
uses this list to degrade gracefully against older server versions (see
capability-tiers.md). A server with no embedder and no LLM configured emits
`["memory"]`. `embedder.state` is one of `ready`, `loading`, `unavailable`
(terminal failure), or `disabled` (no embedder configured at all), present so
a client can distinguish "still warming up" from "not configured" even before
`capabilities` reflects readiness. `limits` (verified against `handlers.rs`)
advertises the server's own `/index/embed` sizing so a client can batch
correctly against the server it's actually talking to; a server predating this
field should be assumed to enforce the old blanket 30s request budget with no
exemption.

### `POST /v1/projects/{project_id}/memory/search`

`SearchRequest { query: String, limit: usize }`: the server encodes the query
internally using its configured embedder; the client never sends a
pre-computed vector.

```json
{ "query": "how does authentication work", "limit": 20 }
```

If the server has no embedder configured, it returns `400`:

```json
{ "error": { "code": "bad_request", "message": "This server has no embedder configured. Semantic memory search is unavailable." } }
```

**Response `429`:** the query embed shares the bounded admission queue in front
of the mutex-serialized embedder with `/index/embed`; once
that queue is full the request is shed immediately rather than queued behind a
running index, with a `Retry-After` (seconds) header:

```json
{ "error": "embedder busy, retry shortly", "state": "busy" }
```

---

## Endpoints added for CLI integration

### `POST /v1/projects/{project_id}/index/embed`

Generate embeddings for code chunks. Called by the CLI during `spelunk index`'s
embed phase. The server encodes each chunk and returns vectors. **The server
does not store the vectors** (the CLI is the only persistent store for index
data).

**Request:**

```json
{
  "chunks": [
    {
      "chunk_id": "abc123",
      "content": "fn parse_config(path: &Path) -> Result<Config> { ... }"
    }
  ]
}
```

`chunk_id` is the CLI's local identifier (blake3 hash of file + offset). It is
opaque to the server and is echoed back so the CLI can match responses to its
local DB rows.

Maximum batch size: **256 chunks per request** (`max_batch_chunks` in
`/v1/health`'s `limits`, above). CLI must split larger batches.

**Response `200`:** vectors as `application/octet-stream`: raw little-endian
`f32` bytes, row-major `[n_chunks × dim]` (896 with the default embedder), in
request order, with **no per-row framing or JSON envelope**. The client maps
response row `i` to request chunk `i` by position. This endpoint also has its
own, much longer request timeout (`embed_request_timeout_secs` in
`/v1/health`'s `limits`, default 1800s) than the rest of the API (30s), since a
legitimate batch can genuinely take minutes on slow or CPU-only hardware. See
`docs/openapi.json` for the full schema.

**Response `400`:** embedder not configured on this server.

**Response `413`:** batch exceeds 256 chunks.

If the server has no embedder, it returns 400:

```json
{ "error": { "code": "bad_request", "message": "index.embed requires an embedder, but this server was built without the native embedder (embed-native feature)." } }
```

**Response `429`:** the embedder is serialized behind a single mutex (GPU
memory limits and the CPU thread budget both rule out running batches
concurrently, see [Embedding CPU thread
budget](../server-setup.md#embedding-cpu-thread-budget)), so a bounded
admission queue sits in front of it. Once that queue is full the request is
shed immediately with `429` and a `Retry-After` (seconds) header, instead of
parking until the caller's own request timeout fires:

```json
{ "error": "embedder busy, retry shortly", "state": "busy" }
```

The CLI's `spelunk index` embed phase retries the same batch after the given
delay rather than shrinking it (queue depth says nothing about batch sizing);
see [`spelunk index`](../commands.md#spelunk-index).

### `POST /v1/projects/{project_id}/explore`

Run an LLM reasoning loop over caller-supplied context. The CLI retrieves
relevant chunks from its local index and sends them alongside the question.
**The server does not store context chunks.**

**Request:**

```json
{
  "question": "Why does the auth middleware bypass token check when api_key is None?",
  "context_chunks": [
    {
      "file": "src/server/mod.rs",
      "start_line": 140,
      "end_line": 165,
      "content": "async fn auth_middleware(...) { ... }"
    }
  ],
  "max_turns": 5
}
```

`context_chunks` is the pre-assembled retrieval context. The CLI is responsible
for fetching this from its local index before calling this endpoint.

**Response:** SSE stream, one JSON event per line:

```
data: {"kind":"thought","content":"The bypass exists to allow unauthenticated local use..."}

data: {"kind":"answer","content":"When no key is configured the server trusts all local callers..."}

data: {"kind":"done"}
```

Event `kind` values: `thought`, `answer`, `done`, `error`.

**Response `503`:** no LLM configured on this server.

---

### `POST /v1/projects/{project_id}/llm/complete`

Run a single LLM completion over caller-supplied messages. This is the generic
inference primitive (ADR-002): it is a 1:1 lift of the `LlmBackend::generate`
trait. The server performs **no** orchestration, adds **no** system prompt, and
stores **nothing**. The CLI owns all prompt assembly (this is how `spelunk
memory harvest` runs after #260: ~2300 LoC of CLI-side orchestration calling
this primitive for raw inference).

**Request:**

```json
{
  "messages": [
    { "role": "system", "content": "You extract decisions from commits." },
    { "role": "user", "content": "<commit batch>" }
  ],
  "max_tokens": 2048,
  "json_schema": { "name": "harvested_decisions", "schema": { } }
}
```

`messages[].role` ∈ `system`|`user`|`assistant`. `max_tokens` is a request; the
server **clamps** it to its configured ceiling. `json_schema` (optional) is the
OpenAI-style `response_format.json_schema`; backends without structured output
ignore it.

**Response `200`:** SSE stream, one JSON event per line:

```
data: {"kind":"token","content":"The "}

data: {"kind":"done"}
```

`kind` ∈ `token` (`content`), `done`, `error` (`code`, `message`).

**Errors:** `400` malformed messages / `max_tokens ≤ 0`; `401` auth; `413` body
too large; `429` per-principal token budget exceeded; `503` no LLM configured:

```json
{ "error": { "code": "llm_unavailable", "message": "llm.complete requires an LLM backend. Configure the chat model on the server." } }
```

**Security (see THREAT-MODEL.md, ADR-002):** Tier-1 + Bearer only; server-side
`max_tokens` ceiling; per-principal rate limit; BYOK key never leaves the server
(decisions #25/#26); prompt-injection isolation is the **caller's**
responsibility — the server adds no system prompt and makes no trust
assumptions about message content. `capabilities` array gains `"llm.complete"`.

### Query embedding for memory — reuse `/index/embed`

`memory add`/`search`/`timeline` obtain **local** query vectors by calling
`/index/embed` with a synthetic chunk (`{"chunk_id":"query:<uuid>","content":
"..."}`); the server echoes the id back opaquely. No dedicated query-embed route
is added. Server-side memory KNN continues to use the text-query
`/memory/search` form above.

### Conflict detection on write

`POST /v1/projects/{project_id}/memory` checks whether a semantically similar
entry already exists (cosine similarity ≥ `--conflict-threshold`, default
`0.92`). If a conflict is detected, the response is **HTTP 409** with a JSON
body:

```json
{
  "stored": true,
  "id": 42,
  "conflicts": [
    { "id": 37, "title": "Previous similar entry", "similarity": 0.97 }
  ]
}
```

The new entry is stored with a `contradicts` edge to the conflicting entry;
409 does not mean the write was rejected, only that the caller should
surface the warning. Configure the threshold with `--conflict-threshold`
(0.0–1.0; `1.0` disables conflict detection).

### Polling for new entries

`GET /v1/projects/{project_id}/memory/since?t=<epoch>&limit=N` returns up to
`N` entries (default 50) created after the given Unix timestamp, sorted
ascending by creation time. The CLI calls this via `spelunk memory since`.

### Streaming entries

`GET /v1/projects/{project_id}/memory/stream` (Server-Sent Events) subscribes
to new entries as they arrive; each line is a JSON object for one newly-added
entry, and the stream persists until the client disconnects. The CLI calls
this via `spelunk memory watch`.

---

## Project identity

All per-project endpoints accept `project_id` as a URL path segment. The CLI
derives this from the `project_id` config field. Convention: `{owner}/{repo}`
(e.g. `acme/my-app`) — any slug is accepted, projects are auto-created on
first write.

---

## OpenAPI commitment

The server publishes its full contract at `GET /api-docs/openapi.json` (wired
via `utoipa`). Every endpoint listed in this document appears there, with
request/response schema components. The `utoipa::ApiDoc` type in
`crates/spelunk-server/src/lib.rs` is the source of truth; extend it whenever
an endpoint changes.

CLI integration tests pull `openapi.json` and assert presence + shape of:

- `paths./v1/health.get.responses.200.content.application/json.schema.properties.capabilities`
- `paths./v1/projects/{project_id}/index/embed.post`
- `paths./v1/projects/{project_id}/memory/search.post.requestBody` includes
  `query: string` (not `embedding: array`).

A snapshot of the generated `openapi.json` is committed under
`docs/openapi.json`. A CI check diffs the committed file against a freshly
generated one on every change; regenerate and commit it in the same PR as
any endpoint change.

---

## Endpoint summary

| Method | Path | Auth | Tier | Notes |
|---|---|---|---|---|
| `GET` | `/v1/health` | None | 0+1 | JSON; unauthenticated liveness probe |
| `GET` | `/api-docs/openapi.json` | None | 0+1 | |
| `GET` | `/v1/projects` | Bearer | 1 | Enumerates every project slug on the instance (by design, see [Trust model](../server-setup.md#trust-model)) |
| `POST` | `/v1/projects/{id}/memory` | Bearer | 1 | May return `409` with a `conflicts` body, see [Conflict detection](#conflict-detection-on-write) |
| `GET` | `/v1/projects/{id}/memory` | Bearer | 1 | `?kind=&limit=&archived=` |
| `GET` | `/v1/projects/{id}/memory/{note_id}` | Bearer | 1 | |
| `DELETE` | `/v1/projects/{id}/memory/{note_id}` | Bearer | 1 | |
| `POST` | `/v1/projects/{id}/memory/{note_id}/archive` | Bearer | 1 | |
| `POST` | `/v1/projects/{id}/memory/{note_id}/supersede` | Bearer | 1 | |
| `POST` | `/v1/projects/{id}/memory/search` | Bearer | 1 | Text query; server embeds it |
| `GET` | `/v1/projects/{id}/memory/since` | Bearer | 1 | `?t=<epoch>&limit=`, see [Polling](#polling-for-new-entries) |
| `GET` | `/v1/projects/{id}/memory/stream` | Bearer | 1 | SSE, see [Streaming](#streaming-entries) |
| `GET` | `/v1/projects/{id}/memory/harvested-shas` | Bearer | 1 | |
| `GET` | `/v1/projects/{id}/stats` | Bearer | 1 | |
| `POST` | `/v1/projects/{id}/index/embed` | Bearer | 1 | Embedding proxy (`application/octet-stream`); also serves memory query-embed via synthetic chunk |
| `POST` | `/v1/projects/{id}/search` | Bearer | 1 | Query-embedding proxy for CLI KNN |
| `POST` | `/v1/projects/{id}/explore` | Bearer | 1 | SSE — LLM reasoning loop |
| `POST` | `/v1/projects/{id}/llm/complete` | Bearer | 1 | SSE — generic inference primitive (ADR-002) |

---

## Implementation status

Every endpoint and the `AuthProvider` trait above are implemented and tested:
`AppState.auth: Arc<dyn AuthProvider>`, `GET /v1/health` returns the JSON shown
above, `memory/search` accepts `{"query": String}`, `index/embed` and
`explore` are live, error responses use the `{"error": {"code", "message"}}`
shape throughout, and the OpenAPI spec at `docs/openapi.json` is kept current
by CI. See [Server setup](../server-setup.md) for deploying a server that
exposes this API to a team, and `crates/spelunk-server/src/handlers.rs` for
the implementation this document is verified against.
