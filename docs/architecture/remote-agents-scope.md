# Remote Agents — v1 Scope

**Status:** Proposal — pending CoS/Johan review
**Author:** Architect
**Date:** 2026-05-28
**Refs:** spelunk memory question #21, decision #19 (0.8.0 server-default),
demos-and-presentations/first-engineering-demo-notes.md,
docs/architecture/server-api.md, docs/architecture/capability-tiers.md

---

## 1. The question

Johan post-demo 2026-05-27:

> "At the moment we don't have a story for how we support users that use remote
> agents, whether those remote agents are in the cloud or just in docker on the
> local machine. This is something we need to address fairly soon. I'm not sure
> if its required for v1, but if not its definitely a fast follow."

CoS asks for: (a) what a remote-agent-aware Spelunk looks like, (b) minimum v1
surface vs fast-follow, (c) dependencies on existing WS-2 work.

## 2. What "remote agent" means in Spelunk's model

**A remote agent is an AI coding agent process that does not share a filesystem
or local network with the workstation that owns the code repository.**

We split the population into four shapes (only the last two are "remote"):

| Shape | Agent process | Code checkout | Spelunk CLI runs | Talks to spelunk-server via |
|---|---|---|---|---|
| **R0 — Local-local** | Workstation | Workstation | Workstation (same FS) | loopback / LAN |
| **R1 — Local-containerised** | Local Docker container | Bind-mounted from host | Inside the container | Docker network → host loopback |
| **R2 — Remote-managed** | Cloud (Anthropic CMA, Cursor Background Agent, Devin, GitHub Copilot Workspace, etc.) | Cloud workspace (ephemeral) | Inside that workspace | Public internet → cloud-api |
| **R3 — Self-hosted remote** | User's own VM / k8s pod | That VM / pod | That VM / pod | LAN VPN → spelunk-server, or public → cloud-api |

R0 is the demo path and the only one fully supported today. **R1/R2/R3 are
what this scope proposal addresses.**

### What "remote agent support" is NOT

Locked product stance (research.md, 2026-05-19, post-Cosmos reclassification):
**Spelunk does not run agents. We do not become an agent OS, an agent runtime,
a session-continuity provider, or a productised persona host.**

Therefore, "remote agent support" means:

- **YES:** A remote agent can install the spelunk CLI, point it at a
  spelunk-server (OSS or Cloud), and get the same memory + retrieval surface
  a local agent gets.
- **YES:** Multiple remote agents on the same project see each other's writes
  in near real time via the SSE memory stream.
- **NO:** We do not relay or proxy agent traffic. The server is not a tunnel
  between agents.
- **NO:** We do not provision, schedule, deploy, or manage agent runtimes.
- **NO:** We do not own agent session state, transcripts, or token budgets.

The mental model: spelunk-server is to agents what an LSP server is to
editors — a long-running peer that agents talk to, not a thing that holds the
agents.

## 3. Architectural surface

### 3.1 Connectivity matrix (what changes per shape)

| Concern | R0 (local) | R1 (Docker) | R2 (cloud-managed) | R3 (self-hosted remote) |
|---|---|---|---|---|
| `server_url` | `http://127.0.0.1:7777` | `http://host.docker.internal:7777` | `https://api.spelunk.cloud` | `https://spelunk.acme.internal` |
| Auth | Optional bearer (Tier 1 default) | Same as R0 | OAuth2 / org-scoped API key (WorkOS) | Bearer key, per-org |
| TLS | n/a (loopback) | n/a (loopback over Docker bridge) | required | required |
| Network discovery | trivial | needs DNS hint or env var injection | none — fixed cloud URL | DNS / VPN |
| Code resides on | host | host (bind-mount) | cloud workspace | remote host |
| Index DB resides on | host | container or bind-mount | cloud workspace | remote host |
| Memory store of record | local git-notes + (optional) server | same | cloud-api (with optional git-notes echo) | self-hosted server |

**Key observation:** R1/R3 are already structurally supported by the existing
Tier 0/Tier 1 model. They just need documentation, defaults, and (R1) a
network-discovery convention. R2 is the genuinely new surface — it requires
cloud identity, cross-org isolation, and a stable public endpoint.

### 3.2 What an R1 (local Docker) agent needs

The whole story: env var + bind-mount + working CLI inside the container.

- `SPELUNK_SERVER_URL=http://host.docker.internal:7777` (Mac/Windows Docker
  Desktop) or `http://172.17.0.1:7777` (Linux default bridge).
- `SPELUNK_SERVER_KEY=...` if the host server requires auth.
- Bind-mount the repo so file paths in memory entries make sense to both host
  and container.
- Bind-mount `~/.config/spelunk/` (or set `SPELUNK_PROJECT_ID` explicitly) so
  the container CLI knows which project it's talking to.

**No code changes needed for R1.** This is purely a documentation +
defaults exercise. The cost: one new docs page, one `docker run` snippet, and
honouring `host.docker.internal` in the `spelunk check` error message ("did
you mean `http://host.docker.internal:7777`?").

### 3.3 What an R2 (cloud-managed) agent needs

The new surface. Three things that don't exist yet:

1. **Public endpoint with TLS** — `cloud-api` deployment on GCP Cloud Run.
   Status: PR #10 merged 2026-05-25 (managed deployment). Endpoint exists in
   form; not yet wired to spelunk-cli's `server_url`.

2. **Org-scoped identity** — a cloud-managed agent cannot ship a bearer token
   from the user's laptop. It needs an org-scoped credential issued by the
   cloud (short-lived JWT, scoped to one project, with read/write capability
   bits). Status: WorkOS plan complete (decisions #327–#329), implementation
   not started.

3. **Bootstrap UX** — the agent needs a one-line way to acquire a credential
   on first run. The proposal: the user generates a project-scoped "agent
   join token" (web UI) and pastes it into the agent's secrets store; the CLI
   exchanges it for a session token on first call. This is exactly the
   pattern WorkOS supports for service principals.

These three are the v1 cloud story regardless of whether the agent is local
or remote — the only thing "remote" adds is that we can't assume loopback.

### 3.4 What an R3 (self-hosted remote) agent needs

Just R1 over the network. The user already operates a spelunk-server they
trust; they need to:

- expose `:7777` to the remote host (VPN, Tailscale, or direct);
- configure `SPELUNK_SERVER_URL` + `SPELUNK_SERVER_KEY` on the remote;
- accept that TLS is now mandatory (the OSS server currently terminates plain
  HTTP; reverse-proxy via Caddy / nginx is the documented pattern).

**One small code change recommended:** a "trust mode" flag on the OSS server
that warns loudly when it binds to a non-loopback interface without TLS.
This is a docs + warn change, not a feature.

## 4. v1 vs fast-follow

> **Definition.** "v1" here means OSS v1.0 (WS-1) and Cloud private beta
> (WS-2) — the two coding workstreams currently active. Anything called
> "v1" must ship in one of those.

### 4.1 v1 — minimum surface

Three deliverables that gate v1. Each is small.

**V1-1. Document R1 (local Docker) as a first-class path.** No code change.
A page in `docs/` + a `docker run` recipe + an entry in the getting-started
guide. Picked up by Docs Writer.

**V1-2. Cloud-api must serve the same OpenAPI surface that spelunk-server
serves** (capability tiers doc, server-api.md). So that pointing
`server_url=https://api.spelunk.cloud` "just works" from spelunk-cli with no
behavioural difference. This is *already* the WS-2 plan; remote-agent scope
just makes it explicit that R2 = "cloud-api is a drop-in spelunk-server."

**V1-3. Bootstrap UX for project-scoped agent credentials.** This is the
genuine new architectural piece. Required because a cloud-managed agent has
no laptop to copy a bearer token from. Spec to be written as ADR after this
scope is approved. **Depends on WorkOS landing (WS-2 in-flight).**

That's it for v1. Notice what's NOT in v1:

- No "agent registry" on the server.
- No "agent identity" beyond the org/user/api-key principal that already
  exists in `AuthProvider`.
- No relay, tunnel, or coordination primitive beyond the SSE memory stream
  that already exists.
- No Docker discovery magic — we document `host.docker.internal` and stop.

### 4.2 Fast-follow (0.8.x → 0.9.x window)

**FF-1. `spelunk doctor --remote` diagnostics.** A command that probes the
configured `server_url` from inside whatever environment the agent lives in,
and prints a remediation message ("can't reach server: are you in a Docker
container? Try `host.docker.internal`. Are you in a cloud workspace? Check
your join token."). Falls naturally out of the existing `spelunk check`
capability probe.

**FF-2. Short-lived join tokens.** Replace long-lived API keys with token
exchange for cloud-managed agents specifically. Token has a TTL, a scope, a
project pin, and a revocation list. Generated from the web dashboard or via
admin API. Builds on top of V1-3.

**FF-3. Per-agent identity hint in memory entries.** Today, memory entries
record an author (user / api-key). Once multiple agents share one principal,
the dashboard needs to disambiguate. Add an optional `agent_id` header (free
text, e.g. `claude-code:session-abc`) that the server records but does not
enforce. Cheap, additive, makes the multi-agent dashboard real. Server-side
schema change only (one column). **Coordinate with WS-2 audit log work.**

**FF-4. R3 (self-hosted remote) TLS hardening warning.** The "trust mode"
warn-on-bind described in §3.4. Three lines in spelunk-server startup.

### 4.3 Explicitly deferred (not v1, not fast-follow — re-evaluate quarterly)

- **Agent-to-agent direct messaging via spelunk.** Memory entries already
  serve this role; no separate channel needed. If a design partner asks,
  the answer is "publish a memory entry with `kind=intent`."
- **Server-side agent supervision / orchestration.** Locked-out by the
  research.md 2026-05-19 stance.
- **Embedded agent runtime in spelunk-server.** Same.
- **Per-agent rate limiting / quota.** Org-level rate limiting is enough
  until we see abuse.
- **Agent capability advertisement.** YAGNI until at least two harnesses
  advertise different capabilities.

## 5. Dependencies on existing WS-2 work

| WS-2 item | Status | Remote-agent dependency |
|---|---|---|
| Managed deployment on GCP | PR #10 merged | V1-2 depends on this (already shipped) |
| WorkOS identity | plan complete; impl pending | **V1-3 hard-depends** on this |
| Audit log | plan complete; impl pending | FF-3 (per-agent hint) should land *with* this, not after |
| SSE memory stream | shipped (cloud-api stream.rs, OSS PR #293) | No new work — already serves R2/R3 |
| Stripe billing | PR #12 open | Not blocking; agent traffic just counts toward the project's metered operations |
| Cloud-api API parity with spelunk-server | partial | V1-2 makes this explicit: parity is now blocking |

**The critical path is WorkOS.** Without WorkOS, R2 cannot be made safe
(can't issue per-agent credentials). With WorkOS, R2 is a small additive
spec on top.

## 6. Recommendation back to CoS

1. **Remote-agent support is NOT a v1.0 blocker for OSS** (WS-1). The OSS
   release ships with R0 + R1-by-documentation. No code change required in
   `spelunk-oss` to support this.

2. **Remote-agent support IS a Cloud private-beta requirement** (WS-2), but
   the required work is **already in WS-2's plan** under WorkOS identity and
   cloud-api parity. The "remote agent story" is the framing, not new work.

3. **The one new spec we owe is V1-3 (bootstrap UX for agent credentials)**.
   Architect to write this as an ADR once WorkOS lands an end-to-end auth
   flow on cloud-api. Estimated 1–2 days of design work, gated on WorkOS.

4. **Fast-follow** (FF-1 through FF-4) is sized at roughly one implementer
   week, scheduled for the 0.8.x → 0.9.x window after OSS v1.0 tags. None of
   it is structurally novel.

5. **Pressure-list status:** The "Remote agents support" candidate
   workstream in `workstreams.md` can be **closed without opening a new
   workstream**, because the work has been absorbed into WS-1 (docs only)
   and WS-2 (already-planned WorkOS + parity work plus a small ADR).

## 7. Open questions for Johan

1. **Cloud-managed agent matrix scope:** is Anthropic CMA the only R2
   harness we care about for v1? Or should V1-3 cover Cursor Background
   Agents and Devin from day one? (Answer changes whether the bootstrap UX
   is browser-paste or also needs a CLI-only path.)
2. **Docker-network defaults:** should `spelunk check` proactively detect
   "I'm in a container" via cgroup inspection and suggest
   `host.docker.internal`, or is the docs page sufficient? (My take:
   docs first, detect later if user feedback demands it.)
3. **R3 hosting guidance:** do we publish a reference deployment recipe for
   self-hosted spelunk-server-with-TLS (Caddy snippet, systemd unit), or
   leave that to the community? (My take: a single `docs/self-hosting.md`
   page covers 90% of need; it's a half-day of Docs Writer.)

---

## Appendix A. Why this scope is small on purpose

The "remote agents" framing risks dragging us into building an agent runtime,
which the 2026-05-19 stance forbids. By defining remote agents as
**peers we serve, not workloads we host**, the whole problem collapses into
three already-existing primitives:

1. The CLI's existing `server_url` capability probe (Tier 0 ↔ Tier 1).
2. The server's existing `AuthProvider` trait (already designed for
   non-bearer auth strategies).
3. The SSE memory stream (already shipped, already serves multi-agent
   coordination by virtue of being SSE).

Everything else is documentation, defaults, and one credential-bootstrap
ADR.

## Appendix B. Out-of-scope alternatives considered

- **Spelunk as a relay for agent traffic.** Rejected: turns us into a
  message bus and creates a data-residency story we have no business
  having. Memory entries already serve every cross-agent need we've seen.
- **Per-agent MCP endpoint with custom transport.** Rejected: MCP already
  exists on cloud-api (`/v1/projects/{id}/mcp`); adding a remote-only
  variant duplicates surface for no benefit.
- **Docker Compose recipe shipped in spelunk-oss with an "agent" service.**
  Rejected: starts us down the "we run agents" path. The user picks their
  agent harness; we provide the memory backend.
- **VPN/tunnel product (Tailscale-style).** Rejected: outside scope; users
  who need this can run Tailscale themselves.
