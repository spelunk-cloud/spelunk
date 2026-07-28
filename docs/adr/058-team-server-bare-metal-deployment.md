# ADR-058: Recommended team-server deployment is bare-metal + systemd; Docker is a local scaffold

> **Partially superseded by [ADR-066](066-native-tls-in-spelunk-server.md) (2026-07-10).**
> ADR-066 adds in-process HTTPS to `spelunk-server`, reversing the "Native TLS in
> spelunk-server" Non-goal below and revisiting the Docker-vs-bare-metal reasoning
> (a routable TLS bind makes a container reachable, so Docker is no longer
> mechanically excluded). The systemd unit, dedicated user, credential handling,
> and sandboxing decisions in this ADR remain in force.

**Date:** 2026-07-05
**Deciders:** founder (Johan), architect
**Trigger:** founder review on PR #516. The pre-v1.0 server
hardening made `spelunk-server` refuse a non-loopback plaintext bind
*unconditionally*, and the founder review of that PR removed the bundled
reverse-proxy/TLS-sidecar packaging entirely (Caddyfile and
`docker-compose.full.yml` deleted; the proxy remediation text stripped from the
refusal message, ADR-056, and `server.md`). That left `server.md` describing a
Docker quick-start that publishes a port nothing can reach, and no first-class
answer to "how do I actually stand up a shared team server?" This ADR records
the founder's answer.

---

## Context

`spelunk-server` (`crates/spelunk-server/`) is an axum HTTP listener. It serves
two roles (`server.md`): an automatic loopback **inference**
backend on a developer's own machine (never a memory store), and — when a
developer sets an explicit `server_url` — a deployed **team memory** server that
holds a team's shared memory ([ADR-004](004-unified-memory-storage.md),
[ADR-056](056-oss-server-tenancy-model.md)). This ADR is about the second role:
how an operator deploys that shared server.

### What the server does and does not do about transport

The server terminates **plaintext HTTP only**. It has no native TLS: there are
no `--tls`, `--cert`, or `--key-file` flags, and none are planned here (see
Non-goals). Its one transport guardrail is `check_bind_safety`
(`crates/spelunk-server/src/main.rs`):

- A **loopback** bind (`127.0.0.0/8`, `::1`, `localhost`) is always allowed,
  keyed or not. This is the developer's-own-machine case.
- A **non-loopback** bind (`0.0.0.0`, a routable address) is **refused when no
  key is set** — that would be an open, unauthenticated server.
- A non-loopback bind **with** a key is *permitted by the binary* but ships the
  bearer key across the network in cleartext. It is not the posture we
  recommend, and the docs must steer operators away from it.

So the server itself does not require TLS. The requirement that a shared
deployment run over TLS is an **operational** one that the operator satisfies
outside the binary, by putting a TLS terminator in front of a loopback-bound
server. The client half enforces the same contract from the other side:
`server_url` must be `https://` unless it points at loopback
(`server.md` "Client configuration").

### Why Docker cannot host the networked serving path

A shared server needs a same-host TLS terminator that reaches the server on
loopback and presents `https://` to the network. In a container, that shape does
not work:

- `spelunk-server` binds `127.0.0.1` **inside the container's own network
  namespace**. That loopback address is not the host's loopback and is not
  reachable from another container, from the host, or from the network.
- Publishing the port (`-p`/`ports:`) forwards to the container's *routable*
  interface, not to its loopback — so a loopback-bound server publishes a port
  that answers nothing (exactly the broken state PR #516 left `server.md` in).
- The escapes all fail or are unshippable as first-party: a sidecar sharing the
  server's netns (`network_mode: service:…`) is the deleted proxy packaging;
  `network_mode: host` behaves differently (and wrongly) on Docker Desktop vs
  Linux; and container-to-container DNS reaches the routable interface, not
  loopback.

Only a **same-host bare-metal** process can bind the host's real loopback and be
reached there by a same-host TLS terminator. This is the mechanical reason
bare-metal — not Docker — is the deployment vehicle for a networked team server.

This also resolves the [Docker remote-agent loopback](../remote-agents.md)
friction: the fix is to containerize the **agent**, not the
**server**. With the server on bare-metal loopback behind the operator's TLS
terminator, a containerized agent connects to the operator's `https://`
endpoint the same way any other client does — the terminator forwards to
`127.0.0.1` on the host, so the agent never needs to reach the server's raw
loopback itself. It does **not** reach the server over the Docker bridge or
`host.docker.internal`: on native Linux, `host.docker.internal`/the
`docker0` gateway address (`172.17.0.1`) lands on the bridge, and a server
bound to the host's `127.0.0.1` is not listening there (only Docker Desktop's
special-cased gateway, or `--network host` on the agent's container, would
make that path work, and neither is the recommended shape here). The server
stays bare-metal on loopback; the client — containerized or not — always goes
through the `https://` terminator, which is exactly what
[`remote-agents.md`](../remote-agents.md)'s R1 recipe needs to be reworked to
say (see §4).

### What already exists

`self-hosting.md` already documents the correct shape:
server on `127.0.0.1`, an operator's TLS reverse proxy (Caddy or nginx) in
front, a systemd unit, and a client pointed at the `https://` hostname. It is
currently framed as one self-hosting recipe among others. What is missing is
(a) a decision that this is *the recommended* path rather than one option,
(b) a first-party systemd unit for our own binary with the credential and
hardening specifics nailed down, and (c) a docs restructure so `server.md`'s
"Team server" section leads with bare-metal instead of a now-broken Docker
quick-start.

---

## Decision

### 1. Recommended topology

The recommended way to run a shared `spelunk-server` for a team is
**bare-metal, same-host TLS termination**:

1. Run `spelunk-server` as a long-lived process bound to loopback
   (`--host 127.0.0.1`) with a key set (`SPELUNK_SERVER_KEY`).
2. Run an operator-owned TLS terminator (nginx, Caddy, a systemd
   socket-activated proxy, a cloud load balancer — the operator's choice) on the
   **same host**, terminating TLS and forwarding to `127.0.0.1:<port>`.
3. Point each client's `server_url` at the operator's `https://` hostname; the
   shared `SPELUNK_SERVER_KEY` never crosses the network in cleartext.

The tenancy model is unchanged: one instance is one trust domain, the shared key
is the boundary, isolation is separate instances ([ADR-056](056-oss-server-tenancy-model.md)).
The keyed-non-loopback-plaintext bind the binary still *allows* is documented as
a footgun, not a recommended posture: a shared deployment binds loopback and
terminates TLS on the same host.

### 2. Packaging: ship a first-party systemd unit for our own binary; do NOT ship a proxy config

We ship a first-party **`spelunk-server.service`** systemd unit (and the docs to
install it), because a service unit for *our own binary* is first-party surface
we own. We do **not** ship any reverse-proxy configuration (no Caddyfile, no
`nginx.conf`) as a first-party artifact — a bundled third-party proxy config is
out (this is the PR #516 founder decision, and this ADR does not reopen it). The
proxy remains the operator's to own; docs may show a **clearly-marked
operator-owned reference example**, never a shipped file.

The shipped unit binds loopback, runs under a dedicated non-root identity, reads
its key from a credential rather than a world-readable env line, and applies
standard sandboxing. Target unit:

```ini
# /etc/systemd/system/spelunk-server.service  (first-party; ships in the repo)
[Unit]
Description=spelunk-server (team memory)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
# Dedicated, unprivileged identity. See "Provisioning the key" for why we
# prefer a static system user over DynamicUser= here (persistent state dir).
User=spelunk
Group=spelunk

# Key is supplied as a systemd credential, not an Environment= line (which is
# world-readable via `systemctl show`/`/proc`). LoadCredential reads a
# root-only file at 0600 and exposes it at $CREDENTIALS_DIRECTORY/server-key,
# readable only by this unit's process.
LoadCredential=server-key:/etc/spelunk/server-key
ExecStart=/usr/local/bin/spelunk-server \
  --host 127.0.0.1 --port 7777 \
  --db /var/lib/spelunk/spelunk.db
# The binary reads the key from $CREDENTIALS_DIRECTORY/server-key directly (or
# a --key-file path) as a first-class credential source — no ExecStartPre
# shim needed. SPELUNK_SERVER_KEY as an env var remains supported as well, for
# operators who prefer it or run outside systemd (see "Provisioning" below).

Restart=on-failure
RestartSec=5

# Hardening — the server needs only its data dir and loopback.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/spelunk
# Additional sandboxing to apply and test during impl:
#   ProtectKernelTunables=true, ProtectControlGroups=true,
#   RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX, MemoryDenyWriteExecute
#   (verify the last against the candle/native-embedder JIT/mmap needs).

[Install]
WantedBy=multi-user.target
```

`DynamicUser=` is attractive (no manual user management, per-boot UID) but is
**not chosen for the default unit**: the server keeps a persistent SQLite
database under `/var/lib/spelunk`, and the default unit favours a fixed-owner
data dir for operator backup/inspection and the `started_by` UID story the CLI
records. The default unit therefore uses a **static `spelunk` system user**.
Founder-reviewed decision: we additionally ship a **`DynamicUser=` +
`StateDirectory=spelunk` variant as a documented alternative** unit — a
reliability win for operators who prefer systemd-managed per-boot UIDs and are
fine with `StateDirectory=`-managed ownership — alongside, not instead of, the
static-user default (see §4 and "Follow-on implementation work").

### 3. Provisioning and securing `SPELUNK_SERVER_KEY` under systemd

- The key is a high-value bearer credential ([ADR-056](056-oss-server-tenancy-model.md)):
  it grants full admin of every project on the instance.
- **Do not** put it in an `Environment=` line — that value is visible to any
  local user via `systemctl show spelunk-server` and `/proc/<pid>/environ`.
- **Preferred, first-class:** `LoadCredential=server-key:/etc/spelunk/server-key`,
  where `/etc/spelunk/server-key` is a `root:root 0600` file containing the raw
  key. systemd exposes it at `$CREDENTIALS_DIRECTORY/server-key`, and the
  binary reads it **directly** from that path (or a `--key-file` flag pointed
  at an equivalent file outside systemd) — founder-reviewed decision: this is a
  first-class credential-read path in the binary, not a shim. No
  `ExecStartPre`/wrapper is needed.
- **Also supported, kept:** the existing `SPELUNK_SERVER_KEY` environment
  variable. Founder-reviewed decision: env-var support stays alongside the
  credential-file path (e.g. via `EnvironmentFile=` pointing at a `0600`
  root-owned file, or set directly by operators/tooling outside systemd that
  prefer it). The credential path is preferred for systemd deployments; the
  env var remains a fully-supported alternative, not merely a fallback.
- Generate the key with `openssl rand -hex 32`. Rotation = replace the file
  contents and `systemctl restart spelunk-server`, then re-distribute to clients.

### 4. Docs restructure plan (executed post-approval, as impl — not in this ADR)

The doc changes below are the **plan** this ADR commits to; they are carried out
in the implementation PR, not in this ADR's PR:

- **`server.md`** — Restructure the "Team server" section to lead with
  **bare-metal + systemd as the recommended deployment**. Replace the current
  Docker "Quick start"/"Production deployment" framing (which now describes an
  unreachable published port) with: (a) bare-metal recommended path pointing at
  `self-hosting.md`, (b) a clearly-labelled "Docker: local scaffold only"
  subsection stating the minimal compose is for a **loopback/local or
  same-Docker-network agent** use only and is **not** a networked team-server
  deployment. This **supersedes the interim pointer PR #516 left** in
  `server.md`.
- **`self-hosting.md`** — Elevate its existing loopback-plus-proxy-plus-systemd
  recipe to *the* recommended team-server path. Update its systemd section to
  match the first-party unit in §2 (credential-based key, static `spelunk` user,
  fuller hardening) so the shipped unit and the doc agree, and document the
  `DynamicUser=` + `StateDirectory=spelunk` variant alongside it as the
  founder-reviewed alternative. Keep the nginx/Caddy blocks but mark them
  explicitly as **operator-owned reference examples**, not shipped
  configuration.
- **`docker-compose.yml`** — Keep as the minimal local scaffold. Its header
  comment states plainly it is loopback/local-and-same-Docker-network only and
  is not a team-server deployment; it stays free of any proxy service (per
  PR #516).
- **`remote-agents.md`** — Rework the R1 recipe from the raw
  `host.docker.internal:7777` bridge model to the TLS-endpoint model: a
  containerized agent's `server_url` points at the operator's `https://`
  terminator (the same one clients use), not the Docker bridge gateway or
  `host.docker.internal`, which does not reach a host-loopback-bound server on
  native Linux. This is the concrete [remote-agent loopback](../remote-agents.md)
  fix; add a one-line cross-reference that the server-side of a team deployment is
  the bare-metal path in `self-hosting.md`.

### 5. Interaction with the health/limits surface

A bare-metal deployment interacts cleanly with the existing `/v1/health` surface
(`crates/spelunk-server/src/handlers.rs`): `/v1/health` is unauthenticated and
binds-then-answers immediately, so an operator (and the TLS terminator's
health-check) can probe `http://127.0.0.1:7777/v1/health` on the same host
before the native embedder finishes warming. The `started_by` UID field in the
health body reflects the unit's `User=spelunk`, which is the identity the CLI's
loopback auto-discovery guards against reusing — a deployed team server on a
shared host is a distinct identity from a developer's auto-started loopback
server, and that is correct. The `/index/embed` route is already exempt from the
30 s request timeout, so large embed batches over the
proxy are not cut off by the router timeout; the operator's proxy
`proxy_read_timeout`/equivalent must be set generously to match (the
`self-hosting.md` nginx example already does this for the SSE stream). If a
`limits` object is later added to `/v1/health` (e.g. an embed batch/timeout
budget), a bare-metal operator surfaces it the same way — same-host, over
loopback, then via the proxy — no deployment-specific handling is needed; this
ADR does not depend on that field existing.

---

## Non-goals

- **Native TLS in `spelunk-server`.** ~~Adding `--tls`/`--cert` and an in-process
  TLS stack is explicitly **out of scope**.~~ **Superseded by
  [ADR-066](066-native-tls-in-spelunk-server.md):** the server now terminates
  HTTPS itself via `--tls-cert`/`--tls-key`. ADR-058's caveat that this "would be
  its own ADR if ever pursued" is exactly what ADR-066 is. Certificate lifecycle
  automation (ACME) remains deferred there.
- **Reopening the Docker-vs-bare-metal or the bundled-proxy question.** Both are
  settled (founder, PR #516). This ADR designs the bare-metal path; it does not
  re-litigate the choice.
- **Per-project ACLs / multi-tenancy on one instance.** Unchanged from
  [ADR-056](056-oss-server-tenancy-model.md); isolation is separate instances.
- **A packaged installer beyond the systemd unit** (`.deb`/`.rpm`, an
  `install-server.sh`). Possible future work; the initial deliverable is the
  unit file plus documented install steps.

---

## Rationale

| Option | Considered | Rejected because |
|---|---|---|
| **Bare-metal + operator TLS terminator, recommended; first-party systemd unit; Docker as local scaffold (chosen)** | ✅ | Only a same-host bare-metal process can bind the host's real loopback and be reached by a same-host TLS terminator — the shape the trust model and `check_bind_safety` already assume. A systemd unit for our own binary is first-party surface we legitimately own, and it lets us ship the credential-handling and hardening as a known-good default instead of leaving every operator to reinvent it. |
| Ship a bundled reverse-proxy (Caddyfile / nginx.conf) as first-party | ✅ | Rejected by the founder on PR #516: a third-party proxy config is not ours to own or support across versions. Reference examples only, clearly operator-owned. |
| Add native TLS to `spelunk-server` | ✅ | Large, separate decision (cert lifecycle, ACME, renewal, cipher policy) that duplicates what a proxy does well. Deferred to a possible future ADR; a Non-goal here. |
| Keep Docker as the recommended team-server deployment | ✅ | Mechanically broken for networked serving: a container's loopback bind is unreachable from off-container, and publishing forwards to the routable interface, not loopback. This is the exact broken state PR #516 left `server.md` in. |
| Docs-only (no shipped unit) | ✅ | Leaves every operator to hand-roll the credential handling and sandboxing, the two things most likely to be done insecurely (key in a world-readable `Environment=` line, server run as root). Shipping a hardened default unit is the higher-leverage, low-surface win, and the unit is our own binary's — not a third party's. |
| `DynamicUser=` in the default unit | ✅ | Attractive (no user management) but complicates a persistent, operator-owned SQLite data dir and the `started_by` UID story. Not the default; ship a `DynamicUser=` + `StateDirectory=spelunk` variant as a documented alternative instead (founder-reviewed decision). |

---

## Consequences

- **Easier:** operators get one recommended, mechanically-correct path and a
  hardened, first-party service unit; `server.md`'s currently-broken Docker
  quick-start stops being the headline for team deployment; the key stops
  landing in world-readable process environments by default.
- **Harder / by design:** the operator still owns the TLS terminator — we ship
  no proxy config, only reference examples. Operators on non-systemd inits
  (Windows service manager, container-orchestrated environments,
  non-systemd Linux) adapt the documented run command themselves; the systemd
  unit is the reference, not the only supported path.
- **Follow-on implementation work (post-approval, tracked as its own task, not
  this ADR):**
  - Teach `spelunk-server` to read the key directly from
    `$CREDENTIALS_DIRECTORY/server-key` (and/or a `--key-file` flag) as a
    first-class credential path, while keeping `SPELUNK_SERVER_KEY` env-var
    support as-is (founder-reviewed decision, §2/§3).
  - Add `spelunk-server.service` to the repo: the static-`spelunk`-user default
    unit **and** a documented `DynamicUser=` + `StateDirectory=spelunk`
    variant (founder-reviewed decision, §2), with the hardening in §2 verified
    against the native embedder's memory/JIT needs (esp.
    `MemoryDenyWriteExecute`).
  - Execute the §4 docs restructure across `server.md`, `self-hosting.md`,
    `docker-compose.yml`, and `remote-agents.md` (including the R1
    TLS-endpoint rework, the concrete remote-agent loopback fix), superseding the
    PR #516 interim pointer.
  - Reconcile `self-hosting.md`'s existing systemd block (currently
    `Environment=`-based, `User=spelunk`) with the credential-based §2 unit.
- **Revisit if:** an operator population appears that genuinely cannot run a
  same-host terminator (fully container-orchestrated shops with no bare-metal
  option) — that would reopen the native-TLS Non-goal as its own ADR.

---

## Security implications

- Moving the key from an inline `Environment=` line to a systemd credential (or
  a `0600` `EnvironmentFile=`/env var for operators who prefer that supported
  alternative) removes the most common exposure: the shared admin key readable
  by any local user via `systemctl show` / `/proc/*/environ`. Since the key is
  a full-admin bearer credential ([ADR-056](056-oss-server-tenancy-model.md)),
  this is a material reduction in local blast radius.
- Running under a dedicated unprivileged `spelunk` user with
  `ProtectSystem=strict`, `NoNewPrivileges=true`, `ProtectHome=true`, and a
  narrow `ReadWritePaths=` limits what a compromised server process can touch —
  it needs only its data dir and loopback.
- The recommended topology keeps the shared key off the wire in cleartext: the
  server binds loopback, the same-host terminator presents TLS, and the client
  contract already refuses a non-loopback `http://` `server_url`. The
  keyed-non-loopback-plaintext bind the binary still permits remains documented
  as a footgun to avoid, not a supported posture.
- No new attack surface is added to the binary: this ADR ships packaging and
  docs, not code paths. Native TLS — which *would* add in-process attack
  surface — is explicitly a Non-goal.

Both open questions raised in review are resolved above (§2/§3): the binary
reads the systemd credential directly (with `SPELUNK_SERVER_KEY` kept as a
supported alternative), and a `DynamicUser=` variant will ship alongside the
static-user default unit.

---

> **Correction (2026-07-12, transport topology, supersedes the
> same-host-terminator reasoning):** In addition to the Non-goal reversal noted
> at the top and in Non-goals, the recommended *topology* described in this ADR
> is superseded by [ADR-066](066-native-tls-in-spelunk-server.md). Wherever this
> ADR recommends the server bind loopback with a **same-host TLS terminator in
> front** (Decision and Security implications), the shipped model instead has the
> server bind a **routable interface and terminate HTTPS itself**
> (`--tls-cert`/`--tls-key`), with nothing in front of it. The plaintext-off-host
> refusal is unchanged and now bimodal: a non-loopback bind is allowed only with
> both TLS and a key. The systemd unit, dedicated user, credential handling
> (extended to the TLS private key), and sandboxing from this ADR remain in
> force; only the "put a terminator in front" shape is retired.
