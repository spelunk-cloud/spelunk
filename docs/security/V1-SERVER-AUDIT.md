# v1.0 Server Security Audit Checklist

**Scope:** `spelunk-server` (`crates/spelunk-server/`), the HTTP attack surface not covered by the CLI security program.  
**Gate:** Must be completed before v1.0 GA. Blocks the v1.0 tag.  
**Date drafted:** 2026-05-17  
**Retargeted:** 2026-07-03 to the OSS server as-built (single-trust-domain tenancy per [ADR-056](../adr/056-oss-server-tenancy-model.md)).

The CLI threat model ([`THREAT-MODEL.md`](THREAT-MODEL.md)) remains valid for the CLI crate. This document covers threats introduced by the HTTP server.

**On this retarget.** The original draft was written against a cloud-shaped server
(Postgres, `org_id` row-level security, `JWT_SECRET`, `sk-sp-` prefixed keys). The
OSS `spelunk-server` is a single-file SQLite service with one shared bearer key and
no identity, org, or role model. Items that assume the cloud shape are relabelled
below as **N/A (cloud-only)** or **N/A by design (ADR-056)**; they are not
unmet requirements. Boxes are ticked only where the sibling fix has merged and the
code was read in this tree to confirm it; each such row cites the file that
implements it. Applicable-but-unmet boxes stay unchecked with the owning task named.

**Legend:** ☑ done (evidence cited) · ☐ applicable, not yet satisfied · N/A relabelled item (with reason).

---

## 1. Injection scanning

The module is `crates/spelunk-server/src/security.rs` (`scan_for_injection`), not the
originally-drafted `src/security/injection.rs` path. It carries 12 patterns, not 8.

| Check | Status |
|---|---|
| `security::scan_for_injection` is called in `POST /v1/projects/{id}/memory` before any DB write | ☑ `handlers.rs::add_note` scans `title`+`body` and returns 422 before the insert |
| No client header or query parameter can bypass the scan | ☑ the scan runs unconditionally inside the handler on the parsed `title`/`body`; there is no bypass field |
| 422 response returns `field` and `category`, never the raw regex | ☑ `handlers.rs` returns `{field, category, message}`; `security.rs` exposes only the `category` name, never the pattern |
| OnceLock pattern compilation verified: no per-request recompilation | ☑ `security.rs::patterns()` compiles once via `OnceLock<Vec<Pattern>>` |
| All default patterns have positive and negative unit tests | ☑ `security.rs` tests cover every pattern positively plus two clean-input negatives |
| Audit log (`tracing::warn!`) fires on every match | ☑ `handlers.rs::add_note` emits a `tracing::warn!` on a match recording the project slug, field, category, and title/body lengths, and never echoes the matched text |

## 2. Bearer-key authentication

The OSS server authenticates with **one shared bearer key** (`ApiKeyAuth`,
`crates/spelunk-server/src/auth.rs`). There is no per-key database record, no key
prefix format, and no per-project key scope. The shared key is the tenancy
boundary ([ADR-056](../adr/056-oss-server-tenancy-model.md)). The rows that assume a
keys-table / scoped-key model are relabelled accordingly.

| Check | Status |
|---|---|
| Configured key never held or compared as plaintext | ☑ `auth.rs::ApiKeyAuth::new` hashes the key with BLAKE3 into a 32-byte digest at construction; the plaintext is not retained |
| Key comparison uses constant-time equality (not `==` on strings) | ☑ `auth.rs` hashes the provided token and compares digests with `constant_time_eq::constant_time_eq_32` |
| `sk-sp-` prefix format validated before any DB lookup | N/A (cloud-only). The OSS server has no `sk-sp-` prefix and no per-key DB lookup; the key is opaque and matched by digest. |
| Revoked/deleted keys rejected immediately (no cache window) | N/A (cloud-only). There is no key store to revoke from; a single shared key is rotated by restarting the server with a new value (ADR-056). |
| Key scope enforced: project-scoped key cannot write to other projects | N/A by design (ADR-056). The shared key grants full access to every project on the instance; isolation is by running separate instances. |

## 3. Tenancy model (single trust domain)

Reframed to the OSS server's ratified model ([ADR-056](../adr/056-oss-server-tenancy-model.md)):
a server instance is a **single trust domain**, and its shared key is the boundary.
There is no `org_id`, no row-level security, and no cross-tenant isolation *within*
one instance. That is intended, not a gap. Projects on one instance are addressed
by a `project_id` slug in the path, which is a routing key, not a security boundary.
Isolation between teams is achieved by running **separate instances**, each with its
own key and database. The rows below are therefore N/A by design.

| Check | Status |
|---|---|
| Every DB query includes an `org_id` filter (no bare table scans) | N/A by design (ADR-056). No `org_id` exists; queries scope by `project_id` slug, which is an addressing key, not an isolation control. |
| RLS enabled and tested: a valid key from org A cannot read org B's entries | N/A by design (ADR-056). SQLite has no RLS and the model has no orgs; one key = one trust domain over all projects. |
| Integration test: cross-org read attempt returns 403, not 404 or 200 | N/A by design (ADR-056). Cross-project access with the shared key is intended behaviour; there is no cross-org boundary to test. |

**Operator guardrail (applicable, verify before GA):** the server must emit a
startup notice on a keyed, non-loopback bind stating that every keyholder is a full
administrator of all projects and that separate instances are the way to isolate.
Status: ☑ implemented in `main.rs::should_warn_single_trust_domain` /
`warn_single_trust_domain`, which fire the ADR-056 notice on a keyed non-loopback bind.

**Transport guardrail (applicable, ☑ implemented):** the shared key is a bearer
credential that must not travel in cleartext (ADR-056). `main.rs::check_bind_safety`
therefore refuses **unconditionally** to bind a non-loopback address over plaintext
HTTP, covering both the keyless case (an open, unauthenticated server) and the keyed
case (the bearer key would cross the network in the clear). The refusal names the
interface/port. There is no
override; the only supported posture for a shared server is to bind loopback.
Covered by unit test
`non_loopback_with_key_plaintext_is_refused_unconditionally`.

## 4. Input validation

Path params in the OSS server are **project slugs** (e.g. `usercise/spelunk`), not
UUIDs, so the "malformed UUID" row is reframed as a slug length/sanity cap.

| Check | Status |
|---|---|
| Title field: max 500 characters enforced at route handler | ☑ `handlers.rs` `MAX_TITLE_LEN = 500`, returns 400 on violation |
| Body field: max 50 000 characters enforced at route handler | ☑ `handlers.rs` `MAX_BODY_LEN = 50_000`, returns 400 on violation |
| Path param (project slug) validated; an over-long slug returns 400, not 500 | ☑ `handlers.rs` `MAX_SLUG_LEN = 200`, enforced in `require_project` and the handlers that bypass it (add_note / index_embed / project_search / explore / llm_complete) |
| All SQL uses parameterised queries, no string concatenation | ☑ verified: `db.rs` uses `params!` throughout (no `format!`/concatenation into SQL across `crates/spelunk-server/src/`); FTS5 terms are quoted as literals via `fts5_quote_literal` |

Beyond this table, the input-validation hardening also added a `tower_http` middleware stack (see §DoS in
[`THREAT-MODEL.md`](THREAT-MODEL.md#d--denial-of-service)): `RequestBodyLimitLayer`
(2 MiB), `TimeoutLayer` (30s, exempting `/memory/stream`), `ConcurrencyLimitLayer`
(256), plus IP-keyed rate limiting on `/explore` and `/llm/complete`, and an
embedding-vector-length check against the configured dim.

**`/index/embed` timeout carve-out (PR #513 field-failure
follow-up):** the blanket 30s `TimeoutLayer` above made `/index/embed` unusable — a
legitimate calibrated batch (or even a single oversized chunk on slow/CPU-only
hardware) genuinely needs minutes, and was being killed at 30s regardless of what
the CLI's own client-side timeout allowed. Fixed by giving `/index/embed` its own
long-budget timeout (`EMBED_REQUEST_TIMEOUT`, 1800s, matching the CLI's
`MAX_REQUEST_TIMEOUT` ceiling) instead of `REQUEST_TIMEOUT` — same carve-out
pattern as the `/memory/stream` exemption above, not a blanket removal:
`/index/embed` keeps the same `auth_middleware` + `ConcurrencyLimitLayer` +
`RequestBodyLimitLayer` (2 MiB) + its own `MAX_EMBED_BATCH` (256 chunks) handler
cap, so the DoS surface stays bounded (see `THREAT-MODEL.md`'s updated D-table
row). `/v1/health` now also carries a `limits` object
(`embed_request_timeout_secs`, `max_batch_chunks`, `embedder_token_cap`) so a
client can detect and adapt to a server that pre-dates this fix (absent `limits`
⇒ assume the old 30s/no-exemption profile) instead of assuming its own
calibration always fits whatever server it happens to be talking to.

## 5. SSE stream

The OSS server has one shared key with no per-key revocation and no orgs (ADR-056),
so the revocation-window and cross-tenant rows do not apply.

| Check | Status |
|---|---|
| SSE connection requires a valid key on connection open | ☑ the `/v1/projects/{id}/memory/stream` route is mounted under `auth_middleware` (`lib.rs`), so the key is checked before the stream opens |
| Heartbeat tick re-validates key (revoked key disconnected within 60s) | N/A by design (ADR-056). There is no per-key revocation store; a single shared key is rotated by restarting the server. The keep-alive tick is a transport ping, not a re-auth. |
| SSE events scoped to org (no cross-tenant event possible) | N/A by design (ADR-056). No orgs; the stream is scoped to a `project_id` slug within the single trust domain. |
| Integration test: revoke key, verify SSE connection closes within 60s | N/A by design (ADR-056). No revocation mechanism to test against. |

## 6. Dependencies

Verified against CI (`.github/workflows/security.yml`), which runs on every push
and PR to `main` plus a weekly schedule.

| Check | Status |
|---|---|
| `cargo audit` passes clean (workspace, includes spelunk-server) | ☑ `security.yml` runs `cargo audit`; it fails the job on any unignored advisory |
| `cargo deny` passes (advisories + licenses + bans + sources) | ☑ `security.yml` runs `cargo deny check advisories licenses bans`; `deny.toml` also defines `[sources]` |
| No yanked dependencies in Cargo.lock | ☑ `Cargo.lock` is committed and `cargo audit` reports yanked crates by default, gating the same CI job |

## 7. Configuration and secrets

The OSS server has no `JWT_SECRET` and no `DATABASE_URL`; it is a single-file SQLite
service authenticated by one bearer key. The JWT/database rows are relabelled.

| Check | Status |
|---|---|
| Server refuses to start if `JWT_SECRET` is absent or < 32 bytes | N/A (cloud-only). The OSS server has no JWT; auth is the shared `SPELUNK_SERVER_KEY`. The applicable startup guard is `main.rs::check_bind_safety`, ☑ implemented: it refuses a non-loopback plaintext bind **unconditionally** in both the keyless case (open server) and the keyed case (bearer key in cleartext), naming the interface. Neither refusal has an opt-out. |
| `DATABASE_URL` never logged | N/A (cloud-only). No `DATABASE_URL`; the DB is a local SQLite file path. The applicable rule, that the bearer key is never logged, holds: ☑ `auth.rs` never logs the key or its hash. |
| No secrets in default config files or committed `.env` files | ☑ verified: no committed `.env` and no secrets in the server's default config; `SPELUNK_SERVER_KEY` is supplied by the operator at runtime |
| `.env*` excluded from any server-side file operations | N/A. The server does not walk the filesystem or index files; only the CLI indexer reads project trees (where `.env*` exclusion applies, and is documented in the CLI program). |

## 8. Error responses

| Check | Status |
|---|---|
| 5xx responses do not leak stack traces or internal paths | ☑ `AppError::Internal` returns a fixed generic "Internal server error" 500 regardless of the underlying error text; the one safe case (embedding dim mismatch) is a typed 400 with a fixed message (`lib.rs`, `db.rs`) (PR #509) |
| 422 injection responses reveal category, not pattern | ☑ `handlers.rs` returns `{field, category, message}`; the raw regex is never exposed (see §1) |
| 401 responses consistent (cannot distinguish a missing key from a wrong key) | ☑ `auth.rs` returns the same `AuthError("Unauthorized")` mapped to 401 for both a missing `Authorization` header and a wrong bearer token; there is no 403 path (single shared key), so the missing-vs-wrong distinction does not leak |

<!-- Evidence note (PR #509, merged): AppError::Internal no longer sniffs the error
     Display text for substrings like "mismatch"/"required"; that was the leak. The one
     legitimately safe case (per-project embedding dimension mismatch) is now a typed
     DimensionMismatch error mapped to a 400 with a fixed safe message
     (crates/spelunk-server/src/db.rs, lib.rs); every other Internal error returns a fixed generic
     "Internal server error" 500. The same PR also quoted FTS5 MATCH terms as literals
     (crates/spelunk-core/src/utils/mod.rs fts5_quote_literal, applied in storage/search.rs +
     storage/memory/search.rs), with an embedded-NUL-byte edge case tracked as a separate
     follow-up, and added a uniform MAX_FILE_BYTES gate in
     crates/spelunk-cli/src/cli/cmd/index/parse_phase.rs. -->

---

## Running the checks

```bash
# From the workspace root. Export SPELUNK_SECRET_STORE=file on macOS to avoid
# Keychain prompts during tests.
cargo audit
cargo deny check advisories licenses bans
cargo clippy -p spelunk-server -- -W clippy::all -D warnings

# Server unit + handler tests (SQLite; no external services required)
SPELUNK_SECRET_STORE=file cargo test -p spelunk-server

# Auth, injection-scan, input-cap, and error-mapping tests live in-crate:
#   auth.rs (constant-time key compare), security.rs (injection patterns),
#   handlers.rs (title/body/slug caps, 422 shape, SSE past-timeout).
```

---

## Sign-off

Every **applicable** row must be ☑ (with cited evidence) before the v1.0 tag is
created. **N/A** rows carry no obligation; they record a cloud-only item or an
ADR-056 by-design decision, not an outstanding task.

**State at retarget (2026-07-03):** the only applicable row not yet satisfied was
the §1 injection audit-log (`tracing::warn!` on a match); it is now implemented in
`handlers.rs::add_note` and ticked above. Every applicable row is ☑ with evidence
cited above. Founder sign-off (initials + date) on this retarget is pending review
of this PR.
