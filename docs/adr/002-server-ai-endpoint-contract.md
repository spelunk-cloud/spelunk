# ADR-002: Server AI Endpoint Contract — Generic Inference Primitives vs Per-Command Endpoints

**Status:** Accepted
**Date:** 2026-05-31
**Context:** Issue #260 (strip embedding/LLM from `spelunk-cli`) is blocked. Issue #259 mandates the CLI does no inference — every embedding/LLM call routes through `spelunk-server`. The agreed API contract #261 (CLOSED, `docs/architecture/server-api.md`) covers `/index/embed`, `/memory/search`, `/explore`, `/plan` but has **no endpoint for `spelunk memory harvest`** (~2300 LoC across `harvest.rs`, `harvest_claude.rs`, `harvest_entire.rs`) and **no generic query-embed path** for `memory add`/`search`/`timeline`. The implementer proposed adding a bespoke endpoint per command. Johan posed an open question (#42): rather than one endpoint per command, would two **generic** endpoints — an "llm process" route and an "embed" route — be better, making `spelunk-server` a BFF/proxy for upstream AI integrations?

This ADR answers #42 and specifies the contract that unblocks #260.

---

## Decision

Adopt a **hybrid** surface: **generic inference primitives** for raw, CLI-orchestrated inference, plus a **small fixed set of scoped semantic routes** where the server legitimately owns the orchestration.

### 1. One generic LLM primitive — `POST /v1/projects/{id}/llm/complete`

A single completion endpoint that is a 1:1 lift of the existing `LlmBackend::generate` trait (`crates/spelunk-core/src/llm/mod.rs`):

```
(messages[], max_tokens, json_schema?) → streamed completion tokens
```

Every current LLM call site — `harvest.rs`, `harvest_claude.rs`, `harvest_entire.rs`, `summariser.rs`, and `explore` — already calls exactly this method. The CLI keeps **all** of harvest's prompt orchestration (commit chunking, prompt assembly, structured-output parsing, dedup). The server owns **no** harvest semantics; it borrows only raw inference.

### 2. One generic embed primitive — reuse `POST /v1/projects/{id}/index/embed`

No new embed route. `memory add`/`search`/`timeline` obtain query vectors by calling the existing `/index/embed` (from #261) with a synthetic `chunk_id` that the server echoes back opaquely. `/memory/search` retains its text-query form from #261 for server-side memory KNN.

### 3. Keep `/explore` and `/plan` as scoped, server-owned SSE routes

These already exist in #261 and stay. Their multi-turn reasoning loop and retrieval policy are genuinely server-side concerns; collapsing them into `llm/complete` would push agentic loop logic into the CLI — the opposite of what #259 wants and the opposite of the BFF intent.

**Net surface:** 2 generic primitives (`llm/complete`, `index/embed`) + a fixed semantic set (`memory` CRUD/search, `explore`, `plan`). Not "N bespoke endpoints," and deliberately not literally "two endpoints total."

---

## Why (the case FOR generic primitives)

**The trait is already generic.** `LlmBackend::generate` is `(messages, max_tokens, json_schema) → tokens`. There is no per-command shape to preserve. A bespoke `POST /v1/harvest` would force the server to own prompt-chaining that is pure CLI logic, duplicating ~2300 LoC across the network trust boundary and re-deriving it whenever a CLI command wants inference (harvest today; convention extraction, plan variants, future commands tomorrow).

**Surface area & versioning.** Per-command endpoints grow O(commands); the generic primitive is O(1). A stable primitive is far cheaper to keep in OpenAPI parity with `cloud-api` (decision #28, V1-2: `server_url=https://api.spelunk.cloud` must "just work").

**Where orchestration lives.** Explicit and correct: prompt/extraction orchestration stays in the CLI (it has the local index, the git history, the parsing); the server is a thin inference peer. This matches the project stance that "spelunk does not run agents — it serves them as peers" (decision #28).

---

## Why NOT (the case AGAINST this pick — considered, not rubber-stamped)

A generic "run any prompt" endpoint is a **broader abuse, injection, and cost surface** than a scoped `/harvest` that only accepts a commit range. Anyone holding a valid key can run arbitrary prompts against the operator's metered/BYOK LLM. This is the strongest argument for per-command endpoints, and it is real. We accept the generic primitive **only with** the mitigations below; without them, per-command scoping would win.

A second argument against: a bespoke `/harvest` could let the **server** improve harvest quality centrally (better prompts shipped server-side). We reject this because it contradicts #259/#260's data-ownership and "CLI owns orchestration" model, and because server-side prompt logic cannot see the CLI's local index without the CLI shipping context anyway.

---

## Security / threat-model impact (new trust boundary)

`llm/complete` introduces a **new trust boundary**: a network-facing, free-form inference endpoint. `docs/security/THREAT-MODEL.md` must be updated. Binding requirements:

1. **Auth + tier.** Tier-1 only, `Authorization: Bearer` required, same as all server-mediated routes (#259). No unauthenticated free-form inference.
2. **Hard server-side `max_tokens` ceiling** and **per-principal rate limit** (token budget per `AuthContext.principal`). The client-supplied `max_tokens` is clamped, never trusted upward.
3. **Cost attribution** is per-principal via `AuthContext` (#261 auth trait) — the same granularity a bespoke endpoint would give. No granularity is lost by going generic.
4. **BYOK key never leaves the server.** The client sends prompts; the server holds the upstream provider key. Consistent with decisions #25/#26 (BYOK keys stored as HMAC-SHA256 hashes; real key resolved via Secret Manager in cloud — never passed to the client, never logged).
5. **Prompt injection is the CLIENT's responsibility.** `llm/complete` is a *raw* primitive: the server adds **no** system prompt of its own and makes **no** trust assumptions about message content. Delimiter isolation / angle-bracket escaping of untrusted context stays in the CLI (CLAUDE.md "Prompt structure" decision; threat-model issue #137). The server must not "helpfully" wrap or re-prompt — doing so would create an injection surface it cannot reason about.
6. **No persistence.** `llm/complete` stores nothing (same data-promise as `/explore` in `server-api.md`): messages are request-scoped, never written to the memory DB, never logged in plaintext.

---

## Endpoint contract — `POST /v1/projects/{project_id}/llm/complete`

Run a single LLM completion over caller-supplied messages. Streaming SSE. The server performs no orchestration, adds no system prompt, and stores nothing.

**Auth:** `Authorization: Bearer <token>` (Tier 1).
**Tier-gating:** Tier 1 only. Tier 0 (no server) → the CLI emits the #259 locked-feature error; the endpoint is simply absent from the capabilities list.

**Request:**

```json
{
  "messages": [
    { "role": "system", "content": "You extract decisions from commits." },
    { "role": "user", "content": "<commit batch>" }
  ],
  "max_tokens": 2048,
  "json_schema": {
    "name": "harvested_decisions",
    "schema": { "type": "object", "properties": { } }
  }
}
```

| Field | Type | Required | Notes |
|---|---|:---:|---|
| `messages` | array of `{role, content}` | yes | `role` ∈ `system`\|`user`\|`assistant`. Non-empty. Mirrors `crate::llm::Message`. |
| `max_tokens` | integer | yes | Client request; **server clamps** to its configured ceiling. |
| `json_schema` | object | no | OpenAI-style `response_format.json_schema`. Backends that don't support structured output ignore it (matches current trait contract). |

**Response `200`:** `Content-Type: text/event-stream`, one JSON object per `data:` line:

```
data: {"kind":"token","content":"The "}
data: {"kind":"token","content":"auth "}
data: {"kind":"done"}
```

| `kind` | Fields | Meaning |
|---|---|---|
| `token` | `content: string` | One streamed completion fragment. Concatenate in order. |
| `done` | — | Stream complete. Always terminal on success. |
| `error` | `code: string`, `message: string` | Terminal on failure mid-stream. |

**Error responses** (before the stream opens) use the standard envelope `{"error":{"code","message"}}`:

| HTTP | `code` | When |
|---|---|---|
| 400 | `bad_request` | `messages` empty / malformed; `max_tokens` ≤ 0. |
| 401 | `unauthorized` | Bearer token missing/invalid. |
| 413 | `payload_too_large` | Request body exceeds the server content-length limit. |
| 429 | `rate_limited` | Per-principal token budget / rate limit exceeded. |
| 503 | `llm_unavailable` | No LLM configured on this server (mirrors `/explore` 503). |

`503` body:

```json
{ "error": { "code": "llm_unavailable", "message": "llm.complete requires an LLM backend. Configure the chat model on the server." } }
```

**Capability probe.** `GET /v1/health` `capabilities` array gains `"llm.complete"`. The CLI uses this to gate `memory harvest` (and any other `llm/complete` consumer) in Tier 1 against older servers.

---

## Query embedding for memory — reuse `/index/embed`

`memory add`/`search`/`timeline` need a query vector. They call the existing `POST /v1/projects/{id}/index/embed` with a single synthetic chunk:

```json
{ "chunks": [ { "chunk_id": "query:<uuid>", "content": "<query or note text>" } ] }
```

The server echoes `chunk_id` back with the vector (it is opaque to the server, per `server-api.md`). No new route, no new schema. `memory search` against the **server-side** memory DB continues to use the text-query `/memory/search` form from #261 (server embeds internally). The synthetic-chunk path is for **local** query vectors only (e.g. `memory add` dedup against the local note store).

> Note for the implementer: prefix synthetic ids (`query:`) so they are trivially distinguishable from real chunk ids in logs/metrics. The server treats them identically.

---

## OpenAPI parity (decision #28, V1-2)

`llm/complete` is added to the `utoipa::ApiDoc` source of truth in `crates/spelunk-server/src/lib.rs` and to the committed `docs/openapi.json` snapshot, with a request schema component and an SSE-event schema. `cloud-api` must add the matching route to preserve parity so `server_url=https://api.spelunk.cloud` works unchanged. A follow-up architect/cloud task is filed for the cloud side.

---

## Consequences

- **#260 unblocks.** `ActiveLlm`/`ActiveEmbedder` leave `spelunk-cli`; the `EmbeddingBackend`/`LlmBackend` traits + `OpenAiCompat` impls + `backends.rs` + `summariser` move to `spelunk-server` (per the relocation plan in the inbox note). `vec_to_blob`/`blob_to_vec` stay in `spelunk-core` (pure). `spelunk-core` drops `reqwest`.
- **harvest** becomes Tier-1-only, routed through `llm/complete`; Tier-0 emits the #259 locked-feature error. No `/harvest` endpoint is added.
- **`server-api.md` is amended**, not replaced: it gains the `llm/complete` row and the synthetic-chunk query-embed note. Nothing in #261 is invalidated.
- **THREAT-MODEL.md** gains the new `llm/complete` trust boundary and its six binding requirements.
- **Cost/abuse risk** is accepted in exchange for a stable O(1) inference surface, gated behind auth + rate limiting + a server-side token ceiling.

## Boundary test

If a proposed new CLI command needs LLM inference and its prompt/orchestration can live in the CLI, it routes through `llm/complete` — do **not** add a new endpoint. Add a scoped route **only** when the multi-turn orchestration or retrieval policy must live server-side (as `/explore` and `/plan` do).

---

## Implementation status

Shipped in v0.8 behind PR #260. `spelunk memory harvest` routes all LLM calls through `llm/complete` and all embedding calls through `index/embed`, per the contract above. Tier-0 (no `server_url` configured) emits an actionable error (`harvest_requires_server`), and `GET /v1/health` reports `"llm.complete"` in `capabilities` when an LLM backend is configured server-side.

### Migration: `lm_studio_url` users

> **Superseded. Kept as the v0.8 record.** Both halves below have since been
> overtaken: the shipped binary refuses a non-loopback plaintext `http://`
> `server_url` (use `https://`, or a loopback host), and `api_base_url` /
> `lm_studio_url` are now parsed but ignored rather than still serving
> `explore` and `index --summarize`. All inference routes through
> `spelunk-server`. For current `server_url` configuration see
> [Team setup](../getting-started.md#team-setup-shared-memory-with-spelunk-server).

If you previously used `lm_studio_url` (or `api_base_url`) in your config for
`spelunk memory harvest`, update `~/.config/spelunk/config.toml`:

```toml
# Before
api_base_url = "http://127.0.0.1:1234"

# After — point at a spelunk-server instance
server_url = "http://your-spelunk-server:7777"
project_id = "your/project"
server_key  = "..."       # if the server requires auth
```

`api_base_url` / `lm_studio_url` continue to work for `spelunk explore` and
`spelunk index --summarize` (local-inference features), but are no longer used
by harvest.

---

**Refs:** GH #260, #261, #259; question #42; decision #89 (this decision); decisions #25/#26 (BYOK), #28 (OpenAPI parity / serve-as-peers), #19 (server-default 0.8.0). Companion living doc: `docs/architecture/server-api.md`.
