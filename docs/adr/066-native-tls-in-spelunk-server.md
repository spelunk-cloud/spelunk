# ADR-066: Native in-process HTTPS for spelunk-server

**Date:** 2026-07-10
**Deciders:** founder (Johan), architect
**Supersedes:** the "Native TLS in spelunk-server" Non-goal in
[ADR-058](058-team-server-bare-metal-deployment.md), and revisits that ADR's
Docker-vs-bare-metal reasoning (both were predicated on the server having no
in-process TLS, which this ADR removes).

## Context

`spelunk-server` speaks plaintext HTTP only. It has no TLS: no `--cert`, no
`--tls-*` flags, no in-process terminator. Its one transport guard,
`check_bind_safety` (`crates/spelunk-server/src/main.rs`), refuses **every**
non-loopback bind, whether keyless (an open server) or keyed (the bearer key
would cross the wire in cleartext). That refusal is unconditional and has no
opt-out (hardened pre-v1.0). Loopback binds are always allowed.

The consequence: the only code-supported way to reach the server from another
machine is to bind loopback and put an operator-owned TLS reverse proxy in front
of it on the same host. That is the shape [ADR-058](058-team-server-bare-metal-deployment.md)
made the recommended team-server deployment, and it is the shape the current
`docs/self-hosting.md`, `docs/server.md`, and the `examples/mdm/` daemons all
document.

The product's intended boundary is simpler than that:

- **Local** server: HTTP, loopback only, no API key required.
- **Remote** server: HTTPS only, API key required.

Recommending a reverse proxy in front of the server breaks that boundary: it
makes reachability depend on a separate component the operator has to install,
own, and keep current, and it means the product ships no first-party answer to
"stand up a server my team can reach." Without in-process HTTPS there is no
first-party remote-server path at all.

This ADR gives the server its own HTTPS so that a remote deployment is
`--host <routable> --tls-cert … --tls-key … + API key` with nothing in front of
it.

### Relationship to ADR-056 (tenancy) and ADR-058 (deployment)

[ADR-056](056-oss-server-tenancy-model.md) is about **tenancy**: one instance is
one trust domain, the shared key is the only authorization boundary, cross-project
access is intended. That is orthogonal to transport, and unchanged here. ADR-056's
consequences already state the server "unconditionally requires TLS for any
non-loopback deployment" but did not fix the mechanism. This ADR supplies the
mechanism (in-process) and keeps the shared key as the sole tenancy boundary.

[ADR-058](058-team-server-bare-metal-deployment.md) chose bare-metal + an
operator proxy because a container's loopback bind is unreachable and only a
same-host process could sit behind a same-host terminator. Native TLS removes
that constraint (see Decision §5). ADR-058's systemd/credential/hardening work
stays valid and is reused.

## Decision

### 1. Add in-process TLS via rustls, using axum-server

`spelunk-server` terminates TLS itself using
[`axum-server`](https://docs.rs/axum-server) with its `tls-rustls` integration
over the existing axum 0.8 router.

- **TLS library: rustls, not native-tls.** rustls (0.23) is already compiled
  into the server today through `reqwest` and `hf-hub` (both use `rustls-tls`),
  so the crypto stack is present already. It is pure-Rust with no OpenSSL /
  SChannel / Secure Transport system dependency, which keeps the ubuntu / macos /
  windows CI matrix uniform. `native-tls` would add a per-platform system TLS
  dependency for no benefit.
- **Crypto provider: `ring`, matching the tree.** The server's dependency tree
  already resolves rustls to the `ring` provider (no `aws-lc-sys`). rustls'
  default `aws-lc-rs` provider pulls `aws-lc-sys`, which needs cmake, a C
  compiler, and NASM on Windows. To avoid adding that build-time toolchain
  requirement, take `axum-server` with `default-features = false,
  features = ["tls-rustls-no-provider"]` and install `ring` as the process
  default provider at startup. This adds **no** new build dependency beyond what
  the server already compiles.

### 2. Cert/key provisioning UX: bring-your-own PEM

Two new flags supply an operator-provided certificate chain and private key as
PEM files:

- `--tls-cert <PATH>` (also `SPELUNK_SERVER_TLS_CERT`) – PEM certificate chain
  (leaf + intermediates).
- `--tls-key <PATH>` (also `SPELUNK_SERVER_TLS_KEY`) – PEM private key.

Both set, or neither. Note the deliberate `--tls-` prefix: the existing `--key`
/ `--key-file` flags are the **bearer API key**, a different secret, and the TLS
flags must not be confused with them.

The operator brings a certificate from wherever they already get one (an
internal CA, `certbot`, a cloud-issued cert). rustls loads it with
`RustlsConfig::from_pem_file(cert, key)`.

### 3. Serve path preserves bind-before-warm ordering

The current server binds its `TcpListener` first and warms the ~339 MB native
embedder on a background task afterward, so `/v1/health` answers immediately
during first-run model download. Native TLS must keep that ordering.
`axum_server::from_tcp_rustls(listener, config)` accepts a pre-bound
`std::net::TcpListener`, so the bind still happens before the embedder warms;
only the accept/serve call changes. `ConnectInfo<SocketAddr>` is preserved via
`into_make_service_with_connect_info::<SocketAddr>()`.

When no TLS flags are set, the server keeps the existing plaintext
`axum::serve(listener, …)` path (loopback local server, unchanged).

### 4. check_bind_safety becomes TLS-aware

The guard learns one new fact – whether TLS is configured – and the rule becomes
a direct encoding of "local = HTTP no key, remote = HTTPS + key":

| Bind | TLS configured | Key set | Result |
|---|---|---|---|
| loopback | any | any | allow (unchanged: local HTTP, no key needed) |
| non-loopback | no | any | refuse (unchanged: no plaintext off-host, keyed or not) |
| non-loopback | yes | no | refuse (remote requires an API key) |
| non-loopback | yes | yes | **allow (new: the remote HTTPS path)** |

Nothing currently permitted is removed: the keyed-non-loopback-plaintext bind
that ADR-058 described as "permitted by the binary" was **already refused** by
the time of this ADR (the pre-v1.0 hardening removed it). The only change is additive –
a non-loopback bind becomes allowed when, and only when, TLS **and** a key are
both configured. Plaintext off-host stays refused with no opt-out.

The refusal messages gain a remedy line pointing at `--tls-cert`/`--tls-key`
rather than at a reverse proxy.

### 5. Revisit ADR-058: Docker is no longer mechanically excluded

ADR-058 rejected Docker for a networked team server on one mechanical ground: a
container binds loopback inside its own network namespace, that loopback is
unreachable from the host or sibling containers, and publishing a port forwards
to the container's routable interface, not its loopback. With in-process TLS the
server binds the container's **routable** interface directly
(`--host 0.0.0.0 --tls-cert … --tls-key …`), and `-p 443:7777` then publishes a
working `https://` endpoint. The blocker ADR-058 cited dissolves.

So this ADR revisits and lifts ADR-058's Docker exclusion: with native TLS,
**both** deployment shapes are mechanically sound and neither needs a proxy:

- **Bare-metal + systemd** stays the reference single-host recipe, but the
  server now binds the routable interface with `--tls-cert`/`--tls-key` instead
  of binding loopback behind an operator proxy. ADR-058's unit, dedicated user,
  credential handling, and sandboxing carry over.
- **Container** returns as a supported team-server vehicle: publish the routable
  TLS port; mount the cert and key; no sidecar, no proxy.

Certificate acquisition and renewal remain the operator's responsibility in both
shapes (this ADR does not add ACME – see Non-goals). This ADR proposes keeping
bare-metal + systemd as the documented default and treating the container path
as supported-and-equal; the final "recommended vs supported" wording is a call
to confirm at review.

### 6. systemd credential treatment for the private key

The TLS private key is a high-value secret and gets the same handling as the
bearer key in ADR-058:

- The **private key** is supplied via `LoadCredential=tls-key:/etc/spelunk/tls-key`
  and read from `$CREDENTIALS_DIRECTORY/tls-key` (or a `--tls-key` path at a
  `root:root 0600` file outside systemd), keeping it out of `systemctl show`
  and `/proc/<pid>/environ`.
- The **certificate chain** is public and needs no credential treatment; it is a
  plain readable path (add it to the unit's `ReadOnlyPaths=` if `ProtectSystem=strict`
  hides it).
- `RestrictAddressFamilies=` and the rest of ADR-058's hardening are unchanged;
  the process now also binds a routable socket, which those families already permit.

### 7. Docs restructure plan (executed post-approval, as impl, not in this ADR)

- **`docs/self-hosting.md`** – replace the loopback-plus-operator-proxy recipe
  (its §1/§2 Caddy and nginx blocks) with the native-TLS recipe: the server binds
  the routable interface with `--tls-cert`/`--tls-key` and a key, no proxy. Keep
  the systemd section, updated to load the private key as a second credential and
  to pass the TLS flags.
- **`docs/server.md`** – the "Team server", "Production deployment", "Non-loopback
  plaintext binds are refused", and "Client configuration" sections drop the
  "put a TLS terminator in front" framing in favour of the server's own HTTPS.
  Keep the plaintext-off-host refusal (still true) and the client rule that
  `server_url` must be `https://` off-loopback (now satisfied by the server
  itself). Re-frame "Docker: local scaffold only" now that a routable TLS bind
  makes a container a real team-server option.
- **`examples/mdm/`** (macOS `.mobileconfig`, Windows service installer, README)
  – switch the daemon arguments from `--host 127.0.0.1` + a proxy note back to a
  routable bind with `--tls-cert`/`--tls-key`, undoing the interim
  proxy-pointer that PR #559 added as a stopgap under the
  no-native-TLS constraint.
- **`docs/remote-agents.md`** and **`packaging/*.service`** – point at the
  server's own `https://` endpoint; reconcile the shipped units with §6.
- **`docker-compose.yml`** – can gain a documented routable-TLS team-server
  profile (cert/key mounted), no longer only a local scaffold.

## Non-goals

- **ACME / Let's Encrypt automation.** Automatic issuance and renewal (HTTP-01 or
  TLS-ALPN-01 challenge handling, account state, renewal scheduling) is a
  materially larger surface than PEM loading and is **deferred**. Bring-your-own
  cert covers the requirement; operators who want automatic certs can still front
  the server with a cert manager if they choose. Revisit in a follow-on ADR only
  if demand appears.
- **mTLS / client-certificate auth.** The bearer API key remains the
  authentication mechanism (ADR-056). TLS here is server-side only.
- **Changing the tenancy model.** Unchanged from ADR-056: one instance, one trust
  domain, shared key is the boundary.
- **Cipher / protocol policy knobs.** Take rustls' safe defaults (TLS 1.2+ via the
  `tls12` feature plus 1.3). No operator-tunable cipher list in this iteration.

## Rationale

| Option | Considered | Rejected because |
|---|---|---|
| **rustls in-process via axum-server, ring provider, BYO PEM (chosen)** | yes | rustls is already in the tree; adds no system TLS dep and, pinned to `ring`, no new build toolchain; `from_tcp_rustls` preserves bind-before-warm; BYO PEM is the smallest UX that unblocks a remote server without a proxy. |
| native-tls (OpenSSL / platform TLS) | yes | Adds a per-platform system TLS dependency and CI variance for no benefit over rustls, which is already compiled in. |
| rustls default (`aws-lc-rs`) provider | yes | Pulls `aws-lc-sys` (cmake + C compiler + NASM on Windows), a new build-time toolchain requirement the tree does not have today; `ring` is already the resolved provider. |
| Keep the operator-proxy-in-front model (status quo, ADR-058) | yes | It is exactly what the founder directive rejects: reachability depends on a separate component in front of the server, breaking the local=HTTP / remote=HTTPS boundary the product wants to own. |
| ACME in-process now | yes | Cert lifecycle, renewal, and challenge handling are a large separable surface. Deferred; BYO PEM ships the capability now. |

## Consequences

- **Easier:** a team-reachable server is first-party and proxy-free –
  `--host <routable> --tls-cert … --tls-key …` + a key. Docker becomes a real
  option again. The local=HTTP / remote=HTTPS boundary is enforced in one place
  (`check_bind_safety`) and matches how the product describes itself.
- **Harder / by design:** the operator still provides and renews the
  certificate (BYO PEM; no ACME yet). A cert with no renewal will expire –
  the docs must say so.
- **New in-process attack surface:** a TLS stack now runs in the binary. This is
  the reason ADR-058 deferred it. Mitigated by using rustls (memory-safe,
  audited) with safe defaults, and by the crypto stack already being present via
  reqwest today.
- **Follow-on implementation work (post-approval, its own task, not this ADR):**
  - Add the `axum-server` dep (`tls-rustls-no-provider` + `ring`), the
    `--tls-cert`/`--tls-key` flags and their env vars, the `ring`
    `install_default()` at startup, and the `from_tcp_rustls` serve branch.
  - Extend `check_bind_safety` per §4 with tests for every row of the table.
  - Ship the private-key credential in the systemd units (§6) and execute the §7
    docs/MDM restructure.
- **Revisit if:** operators broadly want automatic certificates – that reopens
  the ACME Non-goal as its own ADR.

## Security implications

- The remote path now keeps the bearer key off the wire in cleartext without any
  external component: the server encrypts the connection itself, and
  `check_bind_safety` refuses a non-loopback bind unless TLS **and** a key are
  both set. Plaintext off-host stays refused with no override.
- The TLS **private key** is treated as a high-value secret, mirroring the bearer
  key: supplied via a systemd credential (or a `0600` root-owned file), never an
  `Environment=` line, kept out of `systemctl show` / `/proc/<pid>/environ`.
- The tenancy boundary is unchanged (ADR-056): the shared key still grants full
  admin of every project on the instance. TLS protects the key in transit; it
  does not add per-project isolation.
- rustls with the `ring` provider and safe protocol defaults (TLS 1.2+/1.3)
  keeps the added stack memory-safe and conservatively configured; no
  operator-tunable cipher policy is exposed in this iteration.
