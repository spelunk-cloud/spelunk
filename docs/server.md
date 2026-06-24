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
(see [Server mode vs no-server mode](getting-started.md#server-mode-vs-no-server-mode)).
With it set, spelunk never autostarts a server and inference-only features exit
with a clear message instead.

How discovery decides whether to reuse or start a server is documented in
[CLI capability tiers → Loopback auto-discovery](architecture/capability-tiers.md#loopback-auto-discovery).
The `instance_id` and `started_by` UID checks described there are implemented
as of v0.8.0 (PRs #329/#333).

---

## Team server

The rest of this page covers running `spelunk-server` as a **deployed, shared**
service so a team can sync memory. This is distinct from the local-auto server
above: it's long-lived, reachable over the network, and protected by an API key.

## Quick start (Docker)

```bash
# Clone and build
git clone https://github.com/spelunk-cloud/spelunk
cd spelunk

# Start the server (no auth — dev only)
docker compose up -d

# Verify
curl http://localhost:7777/v1/health
# → {"status":"ok","version":"0.8.0","capabilities":["memory"],...}
```

## With an API key (recommended)

```bash
# Generate a key
export SPELUNK_SERVER_KEY=$(openssl rand -hex 32)

# Start
SPELUNK_SERVER_KEY=$SPELUNK_SERVER_KEY docker compose up -d

# Save the key — you'll need to distribute it to your team
echo "SPELUNK_SERVER_KEY=$SPELUNK_SERVER_KEY"
```

## Client configuration

Each developer adds a `.spelunk/config.toml` at the project root (commit it):

```toml
# .spelunk/config.toml — commit this, it's not a secret
server_url = "http://spelunk.internal:7777"
project_id = "my-awesome-app"
```

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
namespace — entries from `project_id = "api"` are invisible to clients
configured with `project_id = "frontend"`.

Projects are auto-created on first write — no registration step required.

## Embedding dimension

All clients writing to the same project must use the same embedding model.
The server records the embedding dimension on the first write and rejects
subsequent writes with a different dimension.

Default: 768 dimensions (EmbeddingGemma 300M).

If your team uses a different model, configure the server at startup:

```bash
docker compose run spelunk-server --embedding-dim 1024
```

Or via compose environment:

```yaml
environment:
  SPELUNK_EMBEDDING_DIM: "1024"
```

## Production deployment

`docker-compose.yml` is the recommended minimal deployment — just
`spelunk-server` plus a named volume for the SQLite database.

Key considerations:
- Put the server behind a VPN or private subnet (the API key is the app-level
  guard; network-level access control is the real security boundary)
- The SQLite WAL-mode database handles 2–20 concurrent writers comfortably
- Back up the volume (`spelunk.db`) with your normal database backup process
- For large teams or heavy write loads, see the plan for Postgres support

## Full stack with Ollama (Linux/NVIDIA only)

`docker-compose.full.yml` adds Ollama for server-side LLM inference. This
requires Linux + NVIDIA GPU + nvidia-container-toolkit. It does not work on
Apple Silicon (Docker runs in a Linux VM without GPU passthrough).

```bash
SPELUNK_SERVER_KEY=your-key docker compose -f docker-compose.full.yml up -d
```

## Running without Docker

```bash
# Build
cargo build --release --bin spelunk-server

# Run
./target/release/spelunk-server \
  --db /var/lib/spelunk/spelunk.db \
  --port 7777 \
  --key your-api-key
```

## API reference

All routes require `Authorization: Bearer <key>` except `/v1/health`.

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
