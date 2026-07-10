# spelunk-server

`spelunk-server` does two jobs. Most users only ever meet the first one:

1. **Local inference server (automatic).** It provides embeddings and LLM
   inference for `spelunk` on your own machine. As of v0.8.0 the CLI starts a
   local instance for you in the background — there is nothing to set up.
2. **Team memory server (optional, deployed).** The same binary, run as a
   long-lived service, lets a team share project memory (decisions, context,
   requirements) without sharing code. Each developer's code index stays local;
   only memory entries travel to the server.

If you just installed spelunk and want it to work, you want the local-auto
section below and nothing else. The team-server material starts at
[Team server](#team-server).

---

## Local server (automatic — no setup)

When you run a command that needs inference — `spelunk init`, a semantic
`spelunk search`, `spelunk explore` — the CLI looks for a server on the loopback
address `127.0.0.1:7777`. If none is running, it starts the bundled
`spelunk-server` in the background, owned by your user, and reuses it for the
rest of the session and future runs. You don't configure anything, and you don't
manage a process. Memory still lives in local git-notes; the local server only
provides inference.

If you ever need to manage it explicitly:

```bash
spelunk server start     # start the local server (no-op if already running)
spelunk server stop      # stop the local server
spelunk server status    # show whether a local server is running and its PID
spelunk server logs      # tail the local server's logs
```

To opt out entirely and keep spelunk fully offline, set `SPELUNK_NO_SERVER=1`
(see [Capability tiers](getting-started.md#capability-tiers-where-inference-and-memory-live)).
With it set, spelunk never autostarts a server and inference-only features exit
with a clear message instead.

### Windows: allow the local listener through the firewall

On Windows, the first time the local server binds its loopback port, Windows
Defender Firewall may show a prompt — **accept it**. If it's blocked, `spelunk`
can't reach the server and quietly drops to text/ast-grep search (you'll see a
one-line "no server reachable" notice). If you dismissed the prompt, run
`spelunk server start` again to re-trigger it, then `spelunk server status` to
confirm the server is reachable.

How discovery decides whether to reuse or start a server is documented in
[CLI capability tiers → Loopback auto-discovery](architecture/capability-tiers.md#loopback-auto-discovery).
The `instance_id` and `started_by` UID checks described there are implemented
as of v0.8.0 (PRs #329/#333).

---

## Team server

The rest of this page covers running `spelunk-server` as a **deployed, shared**
service so a team can sync memory. This is distinct from the local-auto server
above: it's long-lived, reachable over the network, and protected by an API key.

**Recommended: bare-metal + systemd.** Run the binary directly on a host under
systemd, bound to loopback, with an operator-owned TLS terminator
(nginx/Caddy/…) in front of that same loopback bind on the same host. This is
the one mechanically-correct shape for a team-reachable server, because the
server binds `127.0.0.1` unconditionally (see
[Non-loopback plaintext binds are refused](#non-loopback-plaintext-binds-are-refused-no-override)
below) and refuses to serve plaintext off-host — so reaching it from another
machine needs a same-host terminator in front of that loopback bind. Docker
cannot host this: a container's own loopback lives in its private network
namespace and isn't reachable from the host or sibling containers by any of the
usual means (bridge port-publish, Docker Desktop host-mode, container-to-container
DNS all fail to reach it).

**[Self-hosting](self-hosting.md) is the full team-server guide** — it walks
through the loopback bind, the first-party `spelunk-server.service` systemd unit
(hardened, key supplied as a systemd credential), and the Caddy/nginx reference
examples. Start there. The rest of this page covers client configuration, the
trust model, and the CLI/flag reference that path relies on.

## Docker: local scaffold only

`docker-compose.yml` in this repo is a **minimal local scaffold**: it builds
the image and runs `spelunk-server` with a persistent named volume for the
SQLite database. It is **not** a networked or team-serving recipe — the
container binds `127.0.0.1` *inside its own network namespace* (the image's
own default; see the Dockerfile), and nothing in the compose file publishes a
port out of that namespace, so the server is not reachable from the host or
from sibling containers. Use it to run the server process locally (e.g. to
poke at the API by hand); for anything a team or a remote machine needs to
reach, use the bare-metal/systemd path in [Self-hosting](self-hosting.md)
instead.

```bash
# Clone and build
git clone https://github.com/spelunk-cloud/spelunk
cd spelunk

# Generate a key (optional for a purely local scaffold, but matches the
# real deployment's shape if you're using this to test client config)
export SPELUNK_SERVER_KEY=$(openssl rand -hex 32)

# Start
SPELUNK_SERVER_KEY=$SPELUNK_SERVER_KEY docker compose up -d
```

Because the container's loopback isn't reachable from outside its own
network namespace, the only way to talk to this instance is from **inside
that same namespace** — there is no host-reachable port to point a client
at. The runtime image is a minimal Debian base with no `curl`/`wget`
installed, so the practical way to reach it is a separate container that
shares the same network namespace:

```bash
docker run --rm --network container:spelunk-server curlimages/curl \
  curl http://127.0.0.1:7777/v1/health
```

If you want other **sibling containers** (not sharing the exact namespace) to
reach a spelunk-server on the same Docker network — e.g. a containerized
agent, see [Remote agents](remote-agents.md) — run it on a user-defined
bridge network instead of via compose:

```bash
docker network create spelunk-dev
docker run --rm -d --name spelunk-server --network spelunk-dev \
  -v spelunk-data:/data spelunk-server
# other containers on `spelunk-dev` reach it at http://spelunk-server:7777
```

This works because Docker's embedded DNS resolves the container's *address on
the bridge network*, not its loopback — the request never needs to cross into
the container's private loopback namespace. A bare
`docker run -p 7777:7777 ...` of this image, by contrast, will **not** make it
reachable from the host: `-p` forwards host traffic to the container's
network interface, not into its private loopback, so nothing published from
the host ever reaches a loopback-only bind. There is no Docker Compose
recipe in this repo for host- or off-host-reachable serving — use
bare-metal/systemd (see [Self-hosting](self-hosting.md)) for that.

## Client configuration

Each developer adds a `.spelunk/config.toml` at the project root (commit it):

```toml
# .spelunk/config.toml — commit this, it's not a secret
server_url = "https://spelunk.internal.example.com"
project_id = "my-awesome-app"
```

> **`server_url` must be `https://` unless it points at loopback**
> (`127.0.0.1` / `::1` / `localhost`). The CLI attaches your bearer token to
> requests built from this URL, so a non-loopback `http://` config is rejected
> at startup with no override — see [Self-hosting](self-hosting.md) for how to
> put TLS in front of a deployed server. Loopback `http://` (e.g. while
> developing against a server on your own machine) is fine.

Personal config (`~/.config/spelunk/config.toml` — never commit):

```toml
# ~/.config/spelunk/config.toml
server_key = "your-shared-api-key"
```

> The legacy `memory_server_url` / `memory_server_key` keys remain accepted as
> deprecated aliases for `server_url` / `server_key`.

`project_id` is a human-readable slug. If the server routes projects by an
internal UUID (as a team/cloud memory server does), the CLI resolves the slug to
that UUID automatically on first use and caches it in
`.spelunk/cloud-project-id.lock`. You don't need to look the UUID up by hand.
The cache is keyed on the slug, so renaming the project re-resolves it
automatically; set `SPELUNK_NO_SLUG_CACHE=1` to force a fresh lookup. A raw UUID
in `project_id` is used as-is. (See [ADR-005](adr/005-cli-slug-uuid-resolution.md).)

Or use the environment variable:

```bash
export SPELUNK_SERVER_KEY=your-shared-api-key
```

## Migrating existing local memory

If team members have existing local `memory.db` entries, push them to the server:

```bash
# Make sure .spelunk/config.toml is set up first, then:
spelunk memory push
```

This reads your local `memory.db` and sends all active entries to the server.
Archived entries are skipped by default; pass `--include-archived` to push them.

## Multiple projects

One server instance supports multiple projects. Each project has its own
*namespace* — entries from `project_id = "api"` are not mixed with entries
from `project_id = "frontend"`. This is an addressing convenience, **not an
access-control boundary**: see [Trust model](#trust-model) below.

Projects are auto-created on first write — no registration step required.

`GET /v1/projects` enumerates every project slug on the instance. This is
intended behaviour, by design — it is not a data leak to be fixed, it follows
directly from the trust model below.

## Trust model

**A `spelunk-server` instance is a single trust domain.** The shared API key
(`--key` / `SPELUNK_SERVER_KEY`) is the *only* access boundary the server has.
It answers exactly one question — "does this bearer token match the
configured key?" — and nothing more: there is no per-project or per-user
authorization layer. Concretely, holding a server's key grants **full
administrator access to every project on that instance**: list, read, search,
write, supersede, archive, and permanently delete, regardless of which project
slug a request names.

This is a deliberate decision, not an oversight — see
[ADR-056](adr/056-oss-server-tenancy-model.md) for the full rationale. The
project-id in the URL path is an addressing convenience for routing requests
to the right namespace; it was never a security boundary, and this document
says so explicitly so no one has to infer it from behaviour.

**What this means for you:**

- A shared/team server is for **one group that already trusts each other** —
  the same trust you'd extend by giving someone commit access to the repo.
  Don't put memory for two teams or organisations that must not see each
  other's data on one instance.
- **Isolation between teams or projects is achieved by running separate server
  instances** — each with its own key and its own database — not by relying on
  project slugs within one instance. Two groups that must not see each other's
  memory run two servers.
- The server enforces the transport half of this at startup: it refuses any
  non-loopback plaintext bind outright, keyed or keyless (see below), so a
  shared/team deployment binds loopback behind an operator-owned TLS proxy. On
  such an instance every keyholder is a full administrator of every project,
  because the shared key is the only boundary.
- If you need per-project or per-user access control within a single
  instance, this server does not provide it (and is not planned to for
  v1.0 — see ADR-056's "Revisit if" clause). The managed cloud product
  provides organization-scoped isolation if you need that instead.

## Embedding dimension

All clients writing to the same project must use the same embedding model. The
embedding model is fixed product-wide to codefuse-ai/F2LLM-v2-330M (896-dim) and
cannot be selected — a mismatched model silently corrupts semantic search. The
server records the embedding dimension on the first write and rejects subsequent
writes with a different dimension.

Default: 896 dimensions (codefuse-ai/F2LLM-v2-330M, the bundled native embedder).

`--embedding-dim` sets the dimension the server enforces. Change it only to match
an external endpoint whose vectors differ in size — doing so means you are
running a different model at your own risk (the one-model-per-vector-space
invariant no longer holds), not a supported way to swap embedding models:

```bash
docker compose run spelunk-server --embedding-dim 1024
```

Or via compose environment:

```yaml
environment:
  SPELUNK_EMBEDDING_DIM: "1024"
```

## Production deployment

**Bare-metal / systemd is the recommended way to run a team-reachable
`spelunk-server`.** The server itself binds loopback only; running it directly
on the host (rather than in a container) means the operator's own TLS
terminator — nginx, Caddy, whatever's already on the box — can sit in front of
that same loopback bind on the same host and actually be reachable off-host.
See [Self-hosting](self-hosting.md) for the systemd unit and reverse-proxy
recipes.

`docker-compose.yml` (see [Docker: local scaffold only](#docker-local-scaffold-only) above)
is a local scaffold for running the server process itself — useful for local
development or testing — not a substitute for the bare-metal path when the
server needs to be reachable by a team or over a network.

Key considerations for any deployment:
- Putting the server behind a VPN or private subnet is still good
  defense-in-depth (the API key is the app-level guard; network-level access
  control is an additional layer, not a substitute for it)
- The SQLite WAL-mode database handles 2–20 concurrent writers comfortably
- Back up the database file with your normal database backup process
- For large teams or heavy write loads, see the plan for Postgres support

## Running without Docker

```bash
# Build
cargo build --release --bin spelunk-server

# Check version
./target/release/spelunk-server --version
# spelunk-server 0.9.0

# Run
./target/release/spelunk-server \
  --db /var/lib/spelunk/spelunk.db \
  --port 7777 \
  --key your-api-key
```

### Bind and auth flags

| Flag | Env | Default | Purpose |
|---|---|---|---|
| `--host` | (none) | `127.0.0.1` | Interface to bind. Non-loopback plaintext binds are refused, keyed or not (see below); a shared deployment binds loopback behind a TLS proxy. |
| `--port` | (none) | `7777` | Port to bind. |
| `--key` | (none) | unset | Shared bearer API key, passed inline. Visible in the process table — prefer `--key-file` or `SPELUNK_SERVER_KEY`. Leave every key source unset only for a loopback dev server. |
| `--key-file` | (none) | unset | Read the key from a file (whole contents, trimmed). First-class alternative to `SPELUNK_SERVER_KEY`, not a fallback. |
| (none) | `SPELUNK_SERVER_KEY` | unset | Read the key from the environment. Fully supported alongside `--key-file`. |

The key is resolved from, in precedence order: `--key` → `--key-file` →
`SPELUNK_SERVER_KEY` → a systemd `LoadCredential=server-key` (read automatically
from `$CREDENTIALS_DIRECTORY/server-key` when present). A blank value from any
source is ignored and falls through to the next. Under systemd the credential
path is preferred — it keeps the key out of the world-readable process
environment; see [Self-hosting](self-hosting.md).

### Embedding CPU thread budget

On a CPU-only host the bundled native embedder (candle) would otherwise fan a
single embed batch across every core, briefly starving the server's own request
handling (`/v1/health` can go unresponsive during a large index). To leave
headroom, the server caps candle's thread count at startup.

| Env | Default | Purpose |
|---|---|---|
| `SPELUNK_EMBED_THREADS` | `max(1, physical cores − 2)` | CPU threads the native embedder may use. Reserves ~2 cores for request serving. |

Precedence: `SPELUNK_EMBED_THREADS` > an already-set `RAYON_NUM_THREADS` >
the bounded default. A pre-set `RAYON_NUM_THREADS` is respected and never
overridden. The resolved value and its source are logged at startup
(`embed CPU thread budget resolved`). GPU (Metal/CUDA) builds are unaffected.

### Non-loopback plaintext binds are refused, no override

`spelunk-server` refuses to bind a non-loopback address over plaintext HTTP,
whether or not a key is set, and there is no opt-out. With no key that would be
an open, unauthenticated server; with a key the bearer `SPELUNK_SERVER_KEY`
would travel across the network in cleartext. The refusal names the
interface/port and points back at this guidance.

The supported posture is to bind loopback and terminate TLS in a front proxy
(see [Self-hosting](self-hosting.md)). If you need a process outside the host
— including a container — to reach the server, put a reverse proxy (nginx,
Caddy, Traefik) in front of the loopback bind and terminate TLS there; don't
bind the server itself to a routable interface over plaintext.

## API reference

All routes require `Authorization: Bearer <key>` except `/v1/health`, which is
unauthenticated by design (it's the liveness probe used before a client knows
whether a key is even needed) — the CLI never attaches a bearer token to it.

```
GET    /v1/health
GET    /v1/projects
POST   /v1/projects/{project_id}/memory
GET    /v1/projects/{project_id}/memory           ?kind=&limit=&archived=
GET    /v1/projects/{project_id}/memory/{id}
POST   /v1/projects/{project_id}/memory/search
DELETE /v1/projects/{project_id}/memory/{id}
POST   /v1/projects/{project_id}/memory/{id}/archive
POST   /v1/projects/{project_id}/memory/{id}/supersede
GET    /v1/projects/{project_id}/memory/since     ?t=<epoch>&limit=
GET    /v1/projects/{project_id}/memory/stream    (Server-Sent Events)
GET    /v1/projects/{project_id}/memory/harvested-shas
GET    /v1/projects/{project_id}/stats
POST   /v1/projects/{project_id}/index/embed      (embedding proxy — vectors not stored)
POST   /v1/projects/{project_id}/search           (query embedding proxy for CLI KNN)
POST   /v1/projects/{project_id}/explore          (SSE — LLM reasoning loop)
POST   /v1/projects/{project_id}/llm/complete     (SSE — raw LLM completion)
```

`POST /index/embed` accepts a JSON batch of chunks (max 256) and returns the
vectors as `application/octet-stream`: raw little-endian `f32` bytes, row-major
`[n_chunks × dim]` (896 with the default embedder), in request order, with no
per-row framing. The client maps response row `i` to request chunk `i` by
position. The server does not store the vectors — the CLI is the only persistent
store for index data. See `docs/openapi.json` for the full schema.

`/index/embed` has its own, much longer request timeout (1800s) than the rest
of the API (30s) — a legitimate batch can genuinely take minutes on slow or
CPU-only hardware. `GET /v1/health`'s `limits` object advertises the current
server's `embed_request_timeout_secs`, `max_batch_chunks`, and (when the native
embedder is loaded) `embedder_token_cap`, so a client can size its own batching
to the server it's actually talking to; a server predating this field should be
assumed to still enforce the old blanket 30s budget with no exemption.

### Conflict detection

When `POST /v1/projects/{project_id}/memory`, the server checks if a semantically similar entry already exists (cosine similarity >= 0.92). If a conflict is detected, the response is **HTTP 409** with a JSON body:

```json
{
  "stored": true,
  "id": 42,
  "conflicts": [
    { "id": 37, "title": "Previous similar entry", "similarity": 0.97 }
  ]
}
```

The new entry is stored with a `contradicts` edge to the conflicting entry. Clients should log or display this warning. Configure the threshold with `--conflict-threshold` flag (0.0–1.0, default 0.92).

### Polling for new entries

Use `GET /memory/since?t=<epoch>&limit=N` to retrieve entries created after a Unix timestamp:

```bash
spelunk memory since 1700000000
```

Returns up to N entries (default 50) created after the given epoch, sorted ascending by creation time.

### Streaming entries

Use `GET /memory/stream` (Server-Sent Events) to subscribe to new entries as they arrive:

```bash
spelunk memory watch
```

Each line is a JSON object representing a newly added entry. The stream persists until the client disconnects.
