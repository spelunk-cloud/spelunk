# spelunk Threat Model

**Method:** Lightweight threat modeling (STRIDE-informed)  
**Last reviewed:** June 2026 (PR #390: ADR-004 unified memory storage)  
**Reviewed by:** Architect  
**Next review:** v1.0 release or after any new network-facing feature

---

## System Overview

spelunk has two distinct operational modes with different attack surfaces:

### Mode A — Local CLI (default)
1. Walks source trees, parses files with tree-sitter, stores chunks in SQLite
2. Embeds chunks by calling an OpenAI-compatible HTTP endpoint (`api_base_url`, default `http://127.0.0.1:1234`)
3. Runs KNN search over stored embeddings via sqlite-vec
4. Optionally sends context + a user question to an LLM endpoint (`llm_model` / same base URL)
5. Maintains a `memory.db` of structured notes with semantic search. **`memory.db` is the single authoritative memory store at the CLI tier** (ADR-004). All `spelunk memory` operations (add, list, search, timeline, harvest) read from and write to `memory.db`.
6. **When `store_in_git_notes = true` (the default):** each `spelunk memory add` also appends the note as a JSON line to `refs/notes/spelunk` on HEAD (PR #339). Git notes in this namespace travel with the repository on `git push` and are available to anyone who clones the repo — see [git-notes memory](#git-notes-memory-prref-notespelunk) below.

**Auto-discovered loopback spelunk-server (v0.8.0+):** spelunk auto-starts a local `spelunk-server` daemon (bound to `127.0.0.1`) to provide a native embedder and LLM backend. This server is **inference-only**: it receives query text or chunk text for embedding, and completion prompts for LLM calls. It does **not** receive note text for storage and is **not** a memory backend. Only an explicit `server_url` in config (pointing at a team or cloud server) moves the memory store of record away from `memory.db`.

### Mode B — spelunk-server
An axum HTTP API (`src/server/`) that exposes memory CRUD and semantic search over the network:
- Binds to a configurable port; intended for shared team use
- Optional bearer token authentication (`--api-key`; **unauthenticated by default**)
- Accepts pre-computed embedding vectors from clients (clients embed locally, server stores and searches)
- Serves multiple projects via project_id routing
- Exposes: `POST /v1/projects/{id}/memory`, `POST /v1/projects/{id}/memory/search`, DELETE, archive, supersede

### Backend configurability
Both the embedding endpoint and LLM endpoint are configurable via `api_base_url` in
`~/.config/spelunk/config.toml`. **This is not restricted to localhost.** Users may
configure third-party cloud services (OpenAI, Anthropic, Cohere, etc.), which changes
the data-egress threat profile significantly.

---

## Assets

| Asset | Confidentiality | Integrity | Availability |
|-------|:-:|:-:|:-:|
| Source code chunks in index | Medium | High | Medium |
| Credentials accidentally present in source | High | — | — |
| Memory notes (decisions, handoffs) | Medium | High | Medium |
| **git-notes memory (`refs/notes/spelunk`)** | **Medium–High** | Medium | Low |
| Embedding vectors | Low | Medium | Low |
| spelunk config (`~/.config/spelunk/config.toml`) | Medium | High | Medium |
| Server-side memory DB (all projects) | High | High | High |
| Bearer token / API key (server mode) | High | — | — |

**Note on git-notes confidentiality:** Notes may contain architectural decisions, credentials accidentally typed into `--body`, handoff text referencing internal systems, or other context a developer would not ordinarily commit to the repo. If the repo is pushed to a shared or public remote the notes are readable by anyone with clone access.

---

## Trust Boundaries and Data Flows

### Mode A — Local CLI

```
User filesystem
  │
  ├─ spelunk index ─► [secret scanner] ─► SQLite index.db (chunks + vectors)
  │                                              │
  │                                              └─► embed via HTTP ─► api_base_url
  │                                                   (local OR third-party cloud)
  ├─ spelunk ask/search
  │     ├─► embed query via HTTP ─► api_base_url
  │     │    (source code chunks + user query leave the machine if api_base_url is remote)
  │     ├─► KNN search ─► index.db
  │     └─► LLM prompt ─► api_base_url
  │           └─ context: code chunks + spec files + memory notes
  │
  ├─ spelunk memory add ─► memory.db (SQLite, local)  ← single canonical store (ADR-004)
  │                     └─► [git notes append] ─► refs/notes/spelunk on HEAD
  │                                                       │
  │                                                       └─► git push ─► remote (any clone)
  │                                                            ┌──────────────────────────────────────┐
  │                                                            │ TRUST BOUNDARY: local repo → remote  │
  │                                                            │ Notes travel with the repo;           │
  │                                                            │ no secret scan on this path (*)       │
  │                                                            └──────────────────────────────────────┘
  │
  └─ spelunk memory search
        ├─► embed query via HTTP ─► loopback spelunk-server (inference-only)
        │    (query text only; note content stays in memory.db — NOT sent to server)
        └─► KNN search ─► memory.db (local sqlite-vec)
```
(*) `spelunk memory harvest` (harvest_claude.rs) does run `contains_secret` on
harvested text before storing. Direct `spelunk memory add` does **not** — the
note body comes from the user's own command line or `$EDITOR` and is written to
git notes verbatim.

**Memory data-flow rule (ADR-004):** Note text for storage is never sent to the
loopback spelunk-server. For `memory search`, only the query string crosses the
loopback trust boundary (to obtain a query embedding); the KNN search and all
note reads/writes operate on the local `memory.db`. If a team `server_url` is
explicitly configured, memory moves to that server instead — see Mode B.

### Mode B — spelunk-server

```
Client (spelunk CLI / any HTTP client)
  │
  ├─► POST /v1/projects/{id}/memory        — store note + pre-computed embedding
  ├─► POST /v1/projects/{id}/memory/search — KNN search by embedding vector
  ├─► GET  /v1/projects/{id}/memory        — list notes
  └─► DELETE / archive / supersede         — mutate note state
         │
         ▼
  spelunk-server (axum, bound to configured port)
    ├─ auth_middleware (bearer token, optional)
    └─ ServerDb (SQLite, server-local)
```

**Key difference from Mode A:** In server mode, memory content is accessible to anyone
who can reach the server's port. If the server is run without `--api-key` and is
reachable beyond localhost (e.g. on a LAN or cloud VM), all memory is unauthenticated.

---

## Threat Analysis (STRIDE)

### S — Spoofing

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Client impersonates a legitimate spelunk user to the server | B | Medium | High | Bearer token auth — but **optional**; server runs unauthenticated by default. Operators must explicitly pass `--api-key`. |
| Attacker spoofs the embedding/LLM backend to return adversarial responses | A | Low | Medium | No server certificate validation is documented; if `api_base_url` is remote and HTTP (not HTTPS), responses can be intercepted. Recommend HTTPS for any non-localhost backend. |

### T — Tampering

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Malicious chunk content injects SQL | A | Low | High | All DB writes use rusqlite parameterised queries — no string formatting into SQL |
| `memory.db` edited directly to corrupt supersession state | A | Low | Medium | Atomic transactions in `insert_with_supersession()` and `supersede()` (issue #136) |
| Unauthenticated HTTP client corrupts server memory DB | B | Medium | High | Bearer token auth — but optional. Unauthenticated by default. |
| Embedding server returns malformed vectors | A/B | Low | Low | Dimension validation on KNN input; errors surface as HTTP 400 (server) or exit 2 (CLI) |
| **git notes rewritten by another tool or git command, corrupting stored memory** | A | Low | Medium | `spelunk memory add` uses `git notes add -f` (force-replace) per-commit. A concurrent `git notes add` or `git notes prune` from another process could silently drop entries. The git-notes backend is documented as unsuitable for concurrent multi-agent use (#185); the SQLite backend is the recommended default for such workflows. |

### R — Repudiation

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| No record of who created/deleted a memory note on the server | B | Medium | Medium | Server has no per-request audit log. `source_ref` field can record commit SHA but is not required. Consider adding `created_by` / request logging for multi-user deployments. |

### I — Information Disclosure

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Credentials in source code indexed into vector DB | A | Medium | High | `secrets.rs` scanner drops matching chunks before storage; `.env*`/`*.pem`/`*.key` files excluded |
| **Source code sent to third-party embedding service** | A | **High** | **High** | **No mitigation in spelunk itself.** If `api_base_url` points to a cloud service, every indexed chunk (post-secret-scan) is transmitted. Users must be informed via docs. |
| **Memory notes sent to third-party LLM** | A | **Medium** | **High** | **No mitigation in spelunk itself.** `spelunk ask` and `memory harvest` send memory content + code context to the configured LLM endpoint. |
| Server memory accessible without auth | B | Medium | High | No `--api-key` by default; any process that can reach the port reads all notes |
| Server bound to 0.0.0.0 exposes data on LAN/internet | B | Medium | High | Bind address is configurable; default and documentation should recommend `127.0.0.1` unless team use is intended |
| Indexed content contains credentials missed by scanner | A | Medium | Medium | Pattern gaps tracked in #138 |
| **Memory note body contains a credential written to git notes and pushed to a shared/public remote** | A | **Medium** | **High** | **No mitigation on the direct `memory add` path.** The `store_in_git_notes` flag is `true` by default. `contains_secret` is not called in `add.rs` before `append_to_git_notes`. Users must set `store_in_git_notes = false` in config to opt out, or avoid including secrets in note bodies. See [git-notes memory](#git-notes-memory-prref-notespelunk) section. Track: issue to add secret-scan gate on write-through path. |
| **Sensitive architectural context (decisions, handoffs) in git notes exposed on clone to any repo reader** | A | **Medium** | **Medium** | Notes attached to `refs/notes/spelunk` are fetched by `git fetch` when the refspec is included; anyone with clone access reads the full history of notes. **Documentation control only** — users must understand that `store_in_git_notes = true` (default) means notes are as public as the repo. |

### E — Elevation of Privilege

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Path traversal via project_id or note body to read arbitrary server files | B | Low | High | project_id is a DB-assigned integer; note body is stored as-is but never executed. No file reads from user input. |

### D — Denial of Service

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Client floods server with large embedding vectors | B | Low | Medium | No request size limits documented in axum config. Recommend `ContentLengthLimit` middleware for production deployments. |

---

## Prompt Injection

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Indexed source file contains adversarial LLM instructions | A | Low | Medium | XML delimiter isolation in `ask.rs`; angle-bracket escaping of retrieved context (issue #137) |
| Indexed source file steers the `explore` LLM into a `read_file` tool call for an arbitrary path (e.g. `/Users/me/.ssh/id_rsa`, `../../etc/passwd`), exfiltrating file contents via the answer / step log | A | Low | High | `read_file` path-boundary enforcement in `explore.rs` (`resolve_indexed_path`): reject absolute / drive / UNC / NUL inputs, lexically reject `..` escape, require index membership against the `files` allow-list (already ignore/secret-vetted by the indexer), and confirm the canonicalized target stays under the canonical project root (symlink backstop). Denial is a recoverable tool result echoing only the caller-supplied path — never a resolved path or file contents (issue #403) |
| User query contains injection payload | A | Low | Low | Pre-flight check against known patterns (`ask.rs` lines 155–174) |
| Memory note stored via team server contains injection payload, later retrieved in `spelunk ask` context | B | Low | Medium | Applies only when an explicit team `server_url` is configured (Mode B). In Mode A, notes are stored in local `memory.db` — not via the loopback server — so this attack requires access to the user's filesystem. Same XML delimiter isolation applies when notes are included in LLM context; angle-bracket escaping must cover memory context (issue #137). |

**Residual risk:** Pre-flight only blocks known string patterns. Novel injection payloads in indexed content or memory notes could influence the LLM response.

---

## Generic inference endpoint — `POST /v1/projects/{id}/llm/complete` (Mode B, ADR-002)

ADR-002 adds a generic LLM completion primitive to `spelunk-server` so the CLI
can route `spelunk memory harvest` (and future inference-needing commands)
through one stable route instead of a bespoke endpoint per command. This
introduces a **new trust boundary**: a network-facing, free-form inference
endpoint that runs arbitrary caller-supplied prompts against the server's
configured (possibly BYOK, possibly metered) LLM.

This is a deliberately broader surface than a scoped `/harvest` endpoint would
be. The trade-off is accepted **only** with the controls below; they are
binding requirements, not recommendations.

| Threat | STRIDE | Likelihood | Impact | Mitigation (binding) |
|--------|--------|-----------|--------|----------------------|
| Authenticated caller runs arbitrary prompts to burn the operator's LLM budget | D / EoP | Medium | Medium | Tier-1 + Bearer auth required; **per-principal rate limit + token budget**; client `max_tokens` **clamped** to a server-side ceiling (never trusted upward) |
| Caller exfiltrates or abuses a BYOK upstream key | I | Low | High | BYOK key **never leaves the server** — client sends prompts, server holds the upstream key; stored as HMAC-SHA256 hash, resolved via Secret Manager in cloud, never logged (decisions #25/#26) |
| Prompt injection via caller-supplied `messages` | T | Medium | Medium | `llm/complete` is a **raw** primitive: the server adds **no** system prompt and makes **no** trust assumptions. Delimiter isolation / angle-bracket escaping of untrusted context is the **caller's** responsibility (issue #137). The server must NOT wrap or re-prompt content. |
| Completion content or prompts persisted/leaked server-side | I | Low | Medium | No persistence: messages are request-scoped, never written to the memory DB, never logged in plaintext (same data-promise as `/explore`) |
| Unconfigured server invoked | — | Low | Low | `503 llm_unavailable` when no LLM backend configured; endpoint absent from `/v1/health` `capabilities` so the CLI gates it |

**Why generic over per-command (security framing):** a bespoke `/harvest` would
narrow the input shape but would force harvest's ~2300 LoC of prompt
orchestration across the trust boundary into the server, expanding the
server's attack surface and duplicating CLI logic. Keeping orchestration in the
CLI and exposing only a raw, auth-gated, rate-limited, non-persisting primitive
is the smaller *server-side* trust surface, at the cost of a broader *input*
surface — which the controls above contain. See ADR-002 for the full rationale.

**Cost attribution** is per-principal via `AuthContext` (#261 auth trait) — the
same granularity a bespoke endpoint would provide. No attribution granularity is
lost by going generic.

---

## git-notes memory (`refs/notes/spelunk`)

PR #339 introduced a write-through that persists every `spelunk memory add` entry
as a JSON line appended to `refs/notes/spelunk` on HEAD when `store_in_git_notes = true`
(the default). This section models the associated data flows and trust boundaries.

### What is stored

Each note is a single-line JSON object (`NoteRecord`) containing: `id`,
`kind`, `title`, `body`, `tags`, `linked_files`, `created_at`, `status`,
`source_ref`, and schema metadata. The `body` field is the raw user-supplied
text from `--body` or `$EDITOR`.

### How notes propagate

```
spelunk memory add
  └─► append_to_git_notes() in storage/git_notes.rs
        ├─► git notes --ref=spelunk show HEAD   (read existing)
        ├─► combine old + new JSON line
        └─► git notes --ref=spelunk add -f HEAD (write back)

git push [with refs/notes/spelunk in refspec or push.followTags / notes config]
  └─► remote repository — readable by anyone with clone access
```

Git does not push notes by default unless the user explicitly configures
`remote.<name>.push = refs/notes/*` or passes `refs/notes/spelunk` on the
command line. However, spelunk's documentation uses `git push --tags` and
`git push` patterns that do not push notes unless configured — but many CI
systems and IDE integrations push all refs. Users should be aware of their
push configuration.

### Trust boundary

| Boundary | Direction | What crosses it |
|----------|-----------|-----------------|
| Local git repo → git remote | On `git push` (when notes refspec is included) | All `NoteRecord` JSON attached to pushed commits |
| git remote → any clone | On `git clone` / `git fetch` with notes refspec | Same NoteRecord JSON |

### Secret-scanning status on this path

| Code path | Scanner called? | Notes |
|-----------|:-:|-------|
| `spelunk index` (chunk storage) | Yes — `contains_secret()` in `parse_phase.rs` | Credentials dropped before DB write |
| `spelunk memory harvest` (harvest_claude.rs) | Yes — `contains_secret()` before storing | Harvested bodies screened |
| `spelunk memory add` → git-notes write-through | **No** | Body is user-supplied text written verbatim to `refs/notes/spelunk`. No call to `contains_secret()` exists in `add.rs` before `append_to_git_notes()`. |

**Risk:** A user who types `spelunk memory add --title "DB creds" --body "password=s3cr3t"` will
have that credential stored verbatim in `refs/notes/spelunk` and, if the repo is
pushed with notes, the credential is exfiltrated.

### Controls and recommendations

| Control | Status |
|---------|--------|
| Secret scanning on `memory add` write-through path | **Gap — not implemented.** Binding requirement #8 below tracks this. |
| `store_in_git_notes = false` opt-out | Available in `~/.config/spelunk/config.toml`; not the default. |
| Documentation warning that notes travel with the repo | Added in `docs/memory.md` and `SKILL.md` (PR #276). |
| `git push` does not push notes by default | True — but not a reliable control; depends on user's git config. |

---

## Third-Party Backend Risk (all modes)

This section is elevated because the original model assumed local-only backends.

**When `api_base_url` is a third-party service (e.g. `https://api.openai.com`):**

| Data sent | Trigger | Risk |
|-----------|---------|------|
| Source code chunk content (post-secret-scan) | `spelunk index` | Code exfiltration to vendor |
| User query text | `spelunk search`, `spelunk ask` | Query logging by vendor |
| Code context + memory notes | `spelunk ask` | Combined context exfiltration |
| Memory note bodies | `spelunk memory harvest` | Decision/requirement exfiltration |

**Mitigations (documentation, not code):**
- Document the data-egress implications prominently in `docs/getting-started.md` and the `config.toml` comments
- Recommend users set `api_base_url = "http://127.0.0.1:1234"` (local model) in the default config
- Secret scanning reduces but does not eliminate the risk — it only drops chunks matching known credential patterns

**Recommended future control:** Add a `data_classification = "local-only"` config flag that refuses to connect to non-loopback addresses, with an explicit opt-in override.

---

## Out-of-Scope Threats

- Remote code execution via the embedding/LLM server (that server is user/operator-controlled)
- Compromised Rust crate supply chain (covered by `cargo audit`/`cargo deny`)

---

## Security Requirement Derivations

From this threat model, the following requirements are binding:

1. **No SQL string formatting.** All DB operations use rusqlite parameterised queries.
2. **Secret scanner must run before every DB write of chunk content.** Enforced in `parse_phase.rs` and `snapshot.rs`.
3. **LLM context must use XML delimiters** with angle-bracket escaping of all retrieved content (issue #137).
4. **Atomic transactions for memory state transitions** — `supersede()` and `insert_with_supersession()` (issue #136).
5. **CI must gate on `cargo audit` and `cargo deny`.**
6. **spelunk-server documentation must warn** that the server is unauthenticated by default and should only be exposed beyond localhost when `--api-key` is set.
7. **Config documentation must warn** that setting `api_base_url` to a non-local address transmits source code and memory content to that endpoint.
8. **Secret scanner must run on the git-notes write-through path.** `add.rs` must call `contains_secret(body)` (and optionally `contains_secret(title)`) before calling `append_to_git_notes()`. If a match is found, the git-notes write must be skipped (with a `tracing::warn!`) and the primary SQLite write must still succeed. This closes the gap identified in the [git-notes memory](#git-notes-memory-prref-notespelunk) section above. **This is a binding requirement for any release with `store_in_git_notes = true` as the default.**
