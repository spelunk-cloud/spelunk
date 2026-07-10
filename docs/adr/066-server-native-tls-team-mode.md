# ADR-066: Native TLS in `spelunk-server` — Team mode requires HTTPS + key, Local mode stays HTTP + loopback

**Date:** 2026-07-10
**Deciders:** founder (Johan), architect
**Trigger:** founder directive overriding the team-server deployment story in
[ADR-056](056-oss-server-tenancy-model.md) and [ADR-058](058-team-server-bare-metal-deployment.md).
ADR-058 explicitly deferred native TLS as a Non-goal, "its own ADR if ever
pursued" — this is that ADR. It supersedes ADR-058's bare-metal +
operator-owned-reverse-proxy recommendation as *the* team-server path.

---

## Context

`check_bind_safety` (`crates/spelunk-server/src/main.rs`) governs what hosts
`spelunk-server` may bind to. Today:

- A loopback bind (`127.0.0.1`, `::1`, `localhost`) is always allowed, keyed or
  not.
- A non-loopback bind (`0.0.0.0`, a routable address) is refused
  **unconditionally** — keyless (an open, unauthenticated server) and keyed
  alike (the bearer `SPELUNK_SERVER_KEY` would cross the network in
  cleartext). There is no override.

ADR-058 designed around that refusal: it recommends running `spelunk-server`
on loopback with an operator-owned TLS terminator (nginx, Caddy) on the same
host, and explicitly puts native TLS in the binary out of scope ("Non-goals:
Native TLS in `spelunk-server`... would be its own ADR if ever pursued").

The founder has now decided the opposite: the binary itself should terminate
TLS, and that is the only supported team-server shape — no external reverse
proxy.

Verified before writing this ADR: `crates/spelunk-server` has **no TLS support
today** — no `rustls`, `native-tls`, or `axum-server` dependency anywhere in
the crate or its `Cargo.toml`. `axum = "0.8"` is the web framework
(`crates/spelunk-server/Cargo.toml:44`); `axum-server` 0.7's `rustls` feature
targets axum 0.8, so this is an additive dependency, not a framework
migration. `clap` is already built with the `env` feature
(`crates/spelunk-server/Cargo.toml:51`), which the existing `--key`/
`SPELUNK_SERVER_KEY` pattern already relies on.

## Decision

**Two supported bind modes. Strictly bimodal — no partial states.**

| Mode | Host | Transport | API key |
|---|---|---|---|
| **Local** | `127.0.0.1` (loopback) | HTTP | optional (authless allowed) |
| **Team** | `0.0.0.0` / a specific non-loopback interface | HTTPS (native, in-process) | **required** |

A non-loopback bind must have **both** a TLS cert/key configured **and** an
API key set. Missing either is refused — there is still no partial or
opt-out state; a keyed-but-plaintext or TLS-but-keyless non-loopback bind
remains invalid, same as today, just for a different reason (incompleteness
of Team mode rather than an unconditional ban).

### 1. Configuration surface

New flags, mirroring the existing `--key` / `--key-file` / `SPELUNK_SERVER_KEY`
pattern:

- `--tls-cert <path>` / `SPELUNK_TLS_CERT` — path to a PEM certificate
  (full chain).
- `--tls-key <path>` / `SPELUNK_TLS_KEY` — path to the matching PEM private
  key.

Both must be set together (one without the other is a startup config error,
independent of `check_bind_safety`'s host check). No built-in ACME/Let's
Encrypt client in this ADR — the operator supplies a certificate however they
already obtain one (Let's Encrypt via certbot, internal CA, self-signed for a
closed network). An in-process ACME client is a possible future ADR if
operator demand appears; it is not designed here.

### 2. TLS termination

`axum-server` with its `rustls` feature serves the existing `axum::Router` via
`axum_server::bind_rustls` when TLS is configured, in place of the current
`axum::serve` + `tokio::net::TcpListener` path. Local mode (no TLS
configured) keeps the existing plain-HTTP serve path unchanged — this is
additive, not a rewrite of the request-handling stack.

No hot-reload of the certificate in v1: rotating a cert means replacing the
files and restarting the process, the same operational model as rotating
`SPELUNK_SERVER_KEY` today (ADR-058 §3).

### 3. Revised `check_bind_safety` policy

Replaces the unconditional non-loopback refusal:

- **Loopback** (`host_is_loopback(host)`): always allowed, exactly as today —
  key optional, TLS irrelevant (a TLS cert may be configured on a loopback
  bind without complaint, but nothing requires it).
- **Non-loopback**: allowed **only** when both a key is set and a TLS
  cert/key pair is configured. Otherwise refuse, with a message naming
  *specifically* what's missing — three distinct cases, not one generic
  error, so an operator isn't left guessing:
  - no key, no TLS configured → refuse, name both.
  - key set, no TLS configured → refuse, name missing TLS cert/key.
  - TLS configured, no key set → refuse, name missing key.

### 4. `warn_single_trust_domain` becomes reachable again

[ADR-056](056-oss-server-tenancy-model.md)'s single-trust-domain startup
notice (`warn_single_trust_domain` / `should_warn_single_trust_domain` in
`main.rs`) fires on a non-loopback + keyed bind. Under today's unconditional
refusal that state is unreachable at runtime — the process exits before
getting there. Under this ADR, a non-loopback + keyed bind is exactly Team
mode, so the warning is live again, unchanged in content: every keyholder is
still a full administrator of every project on the instance (ADR-056 is not
revisited by this ADR).

### 5. Docs restructure (executed post-approval, as impl — not in this ADR)

- **`docs/self-hosting.md`** — lead with native TLS as the Team-mode path:
  generate/obtain a cert, pass `--tls-cert`/`--tls-key` alongside
  `--host 0.0.0.0` and a key, done. The bare-metal + operator-owned
  reverse-proxy recipe from ADR-058 is **not deleted** — an operator who
  already terminates TLS in front of everything on a host may still run
  `spelunk-server` in **Local mode** behind their own proxy (the proxy's
  existence doesn't touch `spelunk-server`'s own bind, which stays loopback).
  It is demoted from *the* recommended path to *an* alternative for operators
  who prefer centralizing TLS in infrastructure they already run.
- **`docs/server.md`** — document the two modes as a table (as above), the
  new flags/env vars, and the three refusal messages.
- **`Dockerfile` / `docker-compose.yml`** — ADR-058 §"Why Docker cannot host
  the networked serving path" concluded Docker can't host a team server
  because a container's loopback + a same-host proxy don't compose. Native
  TLS **removes that constraint**: a container can bind `0.0.0.0` directly
  with a mounted cert/key and a published port, and now correctly serves
  HTTPS from inside the container's own network namespace. Revisit the
  Docker "local scaffold only" framing as a follow-up task — it is no longer
  categorically true, though not designed in detail here.
- **ADR-056, ADR-058** — append a dated *Superseded-in-part* blockquote each
  (ADR bodies are immutable) pointing at this ADR: ADR-058's "Recommended
  topology" (§1) and its "Native TLS" Non-goal are both superseded; ADR-056's
  tenancy model (single trust domain, shared key is the boundary) is
  unchanged and not superseded.

## Rationale

| Option | Considered | Rejected because |
|---|---|---|
| **Native TLS in `spelunk-server`; strict Local/Team bimodal split (chosen)** | ✅ | Founder directive: a team server is `spelunk-server` itself on `0.0.0.0` over HTTPS with a key — no separate process to stand up, own, and keep patched. Also incidentally un-blocks Docker as a viable team-server host (ADR-058's Docker rejection was specifically about the loopback+proxy shape). |
| Keep ADR-058's bare-metal + operator-TLS-terminator as the only path | ✅ | This is the status quo being overridden. Requires every operator to stand up and maintain a second process (nginx/Caddy) just to run a team server; rejected by founder as unnecessary given `axum-server`+`rustls` makes native termination straightforward. |
| Relax `check_bind_safety` to allow a keyed non-loopback plaintext bind (no TLS at all) | ✅ | This is exactly the hole the original unconditional refusal exists to close — the bearer key would cross the network in cleartext. Never on the table; TLS is mandatory for Team mode, not optional. |
| Built-in ACME (Let's Encrypt) client in this ADR | ✅ | Real future value, but a separable concern (renewal scheduling, HTTP-01/DNS-01 challenge handling, rate limits) that would bloat this decision. Operator-supplied cert/key files ship first; ACME is a candidate follow-up ADR if demand appears. |

## Consequences

- **Easier:** a team server is one binary, one process, cert files, a key,
  and `--host 0.0.0.0` — no reverse proxy to install, configure, or keep
  patched. Docker becomes a legitimate team-server host again (tracked as a
  follow-up, not designed here).
- **Harder / by design:** `spelunk-server` now owns certificate lifecycle
  (the operator still renews and replaces the files; the server does not
  fetch or renew certs itself in v1). No hot-reload — rotating a cert or key
  requires a restart, same operational shape as rotating
  `SPELUNK_SERVER_KEY`.
- **New in-process attack surface:** `rustls` is now linked into the server
  binary. This is accepted — `rustls` is a memory-safe, widely-audited TLS
  implementation, and the alternative (every operator hand-rolling a
  same-host proxy) does not reduce real-world attack surface, it just moves
  it to code we don't control.
- **Follow-on implementation work (tracked as its own task, not this ADR):**
  - Add `axum-server` (`rustls` feature) to `crates/spelunk-server/Cargo.toml`;
    wire `--tls-cert`/`--tls-key`/`SPELUNK_TLS_CERT`/`SPELUNK_TLS_KEY` into
    `main.rs`; switch to `axum_server::bind_rustls` when both are present.
  - Rewrite `check_bind_safety` per §3 above, with three distinct error
    messages.
  - Re-verify `warn_single_trust_domain` fires correctly now that its
    firing condition is reachable again.
  - Execute the §5 docs restructure (`self-hosting.md`, `server.md`,
    Dockerfile/compose framing) and the ADR-056/058 superseding blockquotes.
  - Re-open [spelunk-oss^122](https://agentic-os.johan-0e7.workers.dev/t/spelunk-oss/122)
    once this ADR and its implementation land — that task's docs
    reconciliation needs to be redone against the Team-mode model, not the
    superseded loopback+proxy one.
- **Revisit if:** operator demand appears for automatic certificate
  provisioning (ACME) — a future ADR, not blocking this one.

## Security implications

- Directly closes the concern the original `check_bind_safety` refusal names:
  a non-loopback keyed bind can no longer send the bearer key in cleartext,
  because Team mode requires TLS to be configured before the bind is
  permitted at all. There is no state where a key is set on a non-loopback
  interface without TLS also being active.
- Certificate and key file handling should follow the same guidance as the
  existing `--key-file`: restrictive permissions (`0600`), not committed to
  version control, rotated on suspected exposure.
- No change to [ADR-056](056-oss-server-tenancy-model.md)'s tenancy model —
  the shared key remains the single trust-domain boundary; TLS protects the
  key in transit, it does not add per-project authorization.
- `rustls` (not OpenSSL) is the specified library — memory-safe, no
  C TLS stack in the dependency tree.
