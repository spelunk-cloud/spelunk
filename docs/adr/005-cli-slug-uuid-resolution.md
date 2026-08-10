# ADR-005: CLI slug→UUID resolution for cloud-api dogfooding

**Status:** Proposed  
**Date:** 2026-06-19  
**Deciders:** Architect  
**Trigger:** cloud-api routes all projects by `Uuid` in path
params (`/v1/projects/{project_id}`), but the CLI config carries a human slug
(`project_id = "my-project"` in `.spelunk/config.toml`). The Founder cannot
dogfood against `api.spelunk.cloud` without either (a) hand-pasting the UUID
or (b) a resolution mechanism built into the CLI.

---

## Context

### What cloud-api exposes today

`GET /v1/projects` (cloud-api's `listProjects` operation) returns the
authenticated key's visible projects:

```json
{
  "projects": [
    {
      "id": "018f4e2a-1234-7abc-8def-000000000001",
      "name": "spelunk",
      "slug": "spelunk",
      ...
    }
  ]
}
```

Both `id` (UUID) and `slug` (human-readable) are returned in every `ProjectItem`.
The endpoint is authenticated (any valid API key) and already deployed at
`api.spelunk.cloud`.

**There is no dedicated slug-lookup endpoint.** All downstream routes
(`/v1/projects/{project_id}/memory`, `/memory/since`, `/memory/batch`,
`/memory/stream`, `/mcp`, `/llm/complete`, etc.) declared `{project_id}` as a
UUID: a raw UUID parsed directly out of the path segment.

### What the CLI does today (spelunk-oss)

`Config.project_id` is `Option<String>`. The field is consumed as a slug (a
short human identifier). When `server_url` targets a loopback spelunk-server
(the OSS local-first inference server), the slug is percent-encoded and passed
as the path segment — the local server stores it in `projects.slug` and treats
the slug as the persistence key. That pattern works because the OSS
spelunk-server accepts any string as the project identifier.

**cloud-api does not.** Its `{project_id}` parameter accepts a UUID only; a
non-UUID value (e.g., `"spelunk"`) is rejected 422. So a
config of `project_id = "spelunk"` with `server_url = "https://api.spelunk.cloud"`
fails immediately.

### Why the existing `GET /v1/projects` suffices

The `listProjects` response already carries both `slug` and `id` for each
project. The CLI can:
1. Call `GET /v1/projects` once with the same Bearer key it already has.
2. Find the entry whose `slug` == the configured `project_id` value.
3. Extract its `id` (UUID).
4. Cache the UUID locally so future invocations skip the lookup.

No new cloud-api endpoint is needed.

---

## Decision

> **Amendment: the resolution trigger below is retired for the memory-backend
> path.** The Context above records the situation as it stood on 2026-06-19 and
> is left as provenance. Its load-bearing premise, that cloud-api's
> `{project_id}` path parameter accepted a UUID only and therefore rejected a
> slug, no longer holds: cloud-api's published contract now documents
> `{project_id}` as "Project id (UUID) or slug" on `POST /memory`,
> `GET /memory`, `GET /memory/since`, `POST /memory/batch`, `/edges`, `/graph`
> and `/stream` alike. The self-hosted spelunk-server has always accepted
> either.
>
> The two per-entry routes are the exception: `GET` and `DELETE
> /memory/{entry_id}` are still `Path<(Uuid, Uuid)>`, so the project parameter
> is UUID-only on those two. It does not affect the passthrough, which is why
> they are absent from the list above, but it constrains any later work that
> makes `cloud_first` serve memory against the hosted API.
>
> The mechanism was not merely redundant against the self-hosted peer, it was
> breaking it: a self-hosted spelunk-server answers `GET /v1/projects` in a
> shape the resolver cannot deserialize, so the documented `cloud_first` client
> configuration failed at backend open and took every memory command with it.
> Retiring the resolution is the repair, not a cleanup that followed one.
>
> With both peers slug-accepting, D1's "if it does not parse as a UUID, resolve
> it" branch is never the correct thing to do, so D1 and D6's resolution
> trigger, along with D2's `GET /v1/projects` lookup and D4's
> `.spelunk/cloud-project-id.lock` cache, are gone. `Config.project_id` is now
> passed through verbatim as the project path segment, percent-encoded into a
> single segment, exactly as `CloudSyncClient` has always done. D5's raw-UUID
> behaviour survives as a consequence of that passthrough rather than as a
> special case. `SPELUNK_NO_SLUG_CACHE` no longer does anything.
>
> Live surface: `open_remote_memory_backend_with_bearer` in
> `crates/spelunk-core/src/storage/mod.rs`, and `RemoteMemoryBackend::url` /
> `encode_project_id` in `crates/spelunk-core/src/storage/remote/mod.rs`.
>
> **Second amendment: the per-entry-route exception above is also retired.**
> `GET`/`DELETE /v1/projects/{project_id}/memory/{entry_id}` now accept a
> project slug or a UUID, matching every other memory route, closing the last
> gap this ADR flagged as "constrains any later work that makes `cloud_first`
> serve memory against the hosted API." `CloudApiMemoryBackend` (added to
> serve exactly that later work) no longer rejects a slug `project_id` on
> `get`/`archive`/`supersede` (`per_entry_project` and its call sites removed
> from `crates/spelunk-core/src/storage/remote/cloud_api.rs`). With this gone,
> a slug-configured `cloud_first` project works identically against both
> peers for every memory operation.

### D1. Slug detection and resolution trigger

At the point where the CLI would build a cloud-api URL containing the project
ID (i.e., when constructing any `RemoteMemoryBackend` URL against a non-loopback
`server_url`), detect whether `Config.project_id` is already a UUID:

```
fn looks_like_uuid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
}
```

- If it parses as a UUID → use it directly. No lookup.
- If it does not → treat it as a slug; perform slug→UUID resolution.

This keeps the existing behaviour for users who already have a UUID in their
config (no breaking change).

### D2. Resolution mechanism — `GET /v1/projects`

When resolution is needed the CLI calls:

```
GET {server_url}/v1/projects
Authorization: Bearer {server_key}
```

The response is `ProjectListResponse { projects: Vec<ProjectItem> }`:

```json
{
  "projects": [
    { "id": "uuid", "name": "...", "slug": "...", ... }
  ]
}
```

The CLI finds the entry where `slug == project_id`. If no entry matches, the CLI
returns a clear error:

```
error: project slug "spelunk" not found on api.spelunk.cloud.
       Run 'spelunk projects list' or check .spelunk/config.toml.
```

If the response contains multiple matches (impossible given the slug UNIQUE
constraint, but defensive coding): pick the first match and log a warning.

### D3. Resolution timing — config-load, lazy

Resolution happens lazily, not at `Config::load()` time. It is triggered the
first time the CLI attempts to build a cloud-api project-scoped URL. This keeps
`Config::load()` synchronous and avoids blocking startup with a network call
when the resolved UUID is already cached.

**Concretely:** add a `fn resolve_cloud_project_uuid` (or equivalently, extend
`RemoteMemoryBackend::new`) to:
1. Check the cache (see D4).
2. If cache hit → return immediately.
3. Otherwise → call `GET /v1/projects`, find the slug match, write to cache.

### D4. Caching the resolved UUID

Cache location: `.spelunk/cloud-project-id.lock` in the project directory
(same directory that holds `.spelunk/config.toml`).

File format: a single line containing the UUID string:

```
018f4e2a-1234-7abc-8def-000000000001
```

Cache invalidation policy:
- The file is written once and never revalidated automatically.
- If the user changes `project_id` in their config, the old lock file may
  point at the wrong UUID. The CLI detects this by storing the slug alongside
  the UUID:

```
slug=spelunk
uuid=018f4e2a-1234-7abc-8def-000000000001
```

  On cache read, if the stored `slug` does not match the current `project_id`,
  the cache is discarded and re-resolved.

- `SPELUNK_NO_SLUG_CACHE=1` forces a fresh lookup (useful for testing and for
  recovering from a stale lock after a slug rename).

**Why `.spelunk/cloud-project-id.lock` rather than in-memory or in `config.toml`:**
- In-memory: works but requires resolution on every CLI invocation.
- In `config.toml`: mixes machine-generated UUID with human-authored config;
  bad ergonomics and makes PRs noisy if the file is team-shared.
- `.lock` file: machine-generated, gitignore-able (add `.spelunk/*.lock` to
  `.gitignore`), easy to delete and regenerate, slug-keyed so stale detection
  is trivial.

The `.lock` file should be excluded from version control. The OSS
`.gitignore` template (if one exists) should include `.spelunk/*.lock`.

### D5. Backward compat — raw UUID in config

If `Config.project_id` is already a UUID string, it is used directly. No lookup,
no cache write. This path is zero-cost for users who already have a UUID.

### D6. Error cases

| Situation | CLI behaviour |
|---|---|
| `server_url` is not set | No resolution attempted (offline/loopback path unchanged) |
| `server_url` is loopback | No resolution attempted (OSS server accepts slug directly) |
| `project_id` is a UUID | Use directly, skip resolution |
| `project_id` is a slug, cache hit + slug matches | Use cached UUID |
| `project_id` is a slug, cache miss | Resolve via `GET /v1/projects`, cache, use |
| `project_id` is a slug, slug not found in list | Fatal error with actionable message |
| `GET /v1/projects` times out / 401 / 5xx | Fatal error surfaced to user with HTTP status |
| Project renamed (slug changed) | Old cache slug != new `project_id` → re-resolve automatically |

### D7. No cloud-api changes required

The resolution contract relies entirely on the existing `GET /v1/projects` endpoint
(the `listProjects` operation). No new endpoint, no schema change, no
migration needed on the cloud-api side.

---

## Security implications

- `GET /v1/projects` is already authenticated with the same Bearer key used for
  all other cloud-api calls. The resolution call does not widen the attack surface.
- The cache file contains only a UUID, not the API key or any secret.
- RLS is already enforced by cloud-api: a scoped key only sees projects in its
  scope, so a slug not in the key's scope will correctly return "not found" without
  leaking existence of other projects.
- The `.lock` file must be world-readable only to the local user (no special
  permission requirements beyond the existing `.spelunk/` directory).

---

## Implementer notes (spelunk-oss)

Files to create / modify:

| File | Change |
|---|---|
| `crates/spelunk-core/src/config.rs` | Add `looks_like_uuid(s: &str) -> bool` helper |
| `crates/spelunk-core/src/storage/remote/mod.rs` | Add `resolve_project_uuid` async fn; read/write `.spelunk/cloud-project-id.lock`; call `GET /v1/projects` when needed |
| `crates/spelunk-core/src/storage/remote/wire_types.rs` | Add `ProjectItem` and `ProjectListResponse` deserialization types |
| `.gitignore` (root) | Add `.spelunk/*.lock` |

The resolution logic belongs in `spelunk-core` (not `spelunk-cli`) because
`RemoteMemoryBackend` lives in spelunk-core and is the component that builds
cloud-api URLs.

**`GET /v1/projects` wire types** (add to `remote/wire_types.rs`):

```rust
#[derive(Deserialize)]
pub struct CloudProjectItem {
    pub id: uuid::Uuid,
    pub slug: Option<String>,
    // name, visibility, etc. not needed for resolution
}

#[derive(Deserialize)]
pub struct CloudProjectListResponse {
    pub projects: Vec<CloudProjectItem>,
}
```

**Cache file format** (`.spelunk/cloud-project-id.lock`):

```toml
# Auto-generated by spelunk. Do not edit. Safe to delete — will be regenerated.
slug = "spelunk"
uuid = "018f4e2a-1234-7abc-8def-000000000001"
```

Use TOML for parseability; the two-field struct can be deserialized with
`toml::from_str` using an inline `#[derive(Deserialize)]` struct. No external
crate additions needed.

**Resolution function signature** (sketch — do not treat as implementation code):

```rust
/// Resolve a human project slug to the cloud-api UUID.
///
/// - If `project_id` already parses as a UUID, returns it unchanged.
/// - Otherwise, checks `.spelunk/cloud-project-id.lock` for a cached match.
/// - If cache miss or slug mismatch, calls `GET {server_url}/v1/projects`
///   and persists the result.
pub async fn resolve_cloud_project_uuid(
    project_id: &str,
    server_url: &str,
    server_key: Option<&str>,
    spelunk_dir: &std::path::Path,  // path to .spelunk/ directory
) -> anyhow::Result<uuid::Uuid>;
```

**Loopback guard** — call `spelunk_core::config::is_loopback_url(server_url)` before
attempting resolution; return the slug as-is for loopback servers (those run the
OSS spelunk-server which accepts arbitrary slugs, not cloud-api UUID routing).

---

## Alternatives considered

### A1. Add a `GET /v1/projects/by-slug/{slug}` endpoint to cloud-api

Rejected: not needed. `GET /v1/projects` already returns slugs in every `ProjectItem`.
Adding a dedicated lookup endpoint would be dead weight — the list call is cheap
(single query, key-scoped result set is small), and it avoids adding a new
cloud-api surface before the endpoint contract is needed for another reason.

### A2. Resolve at `Config::load()` time (eager)

Rejected: `Config::load()` is synchronous and is called in many places. Making
it async would ripple across the entire CLI. Lazy resolution on first URL
construction is cleaner.

### A3. Store the resolved UUID in `config.toml`

Rejected: `config.toml` is human-authored and often team-shared (project-level
`.spelunk/config.toml`). Committing a machine-generated UUID would create noise
and confusion. A separate `.lock` file is the conventional approach (see
`Cargo.lock`, `package-lock.json`).

### A4. Require users to put the UUID in config directly

Rejected: this is the status quo that the task is resolving. It requires users
to find the UUID from the cloud-api dashboard or via curl, which is not
dogfood-friendly.

---

## Consequences

- Users can configure cloud dogfooding with `project_id = "spelunk"` (human slug)
  and the CLI resolves the UUID transparently on first use.
- Raw-UUID configs continue to work unchanged (zero-cost path).
- The `.spelunk/cloud-project-id.lock` file is auto-generated; teams should add
  `.spelunk/*.lock` to `.gitignore`.
- Resolution requires one `GET /v1/projects` call on first use per project per
  machine; subsequent calls are cache-hit (disk read only).
- No cloud-api changes needed — the existing `listProjects` endpoint suffices.
