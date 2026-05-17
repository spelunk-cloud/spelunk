# v1.0 Server Security Audit Checklist

**Scope:** spelunk-server (crates/spelunk-server after workspace restructure) — new attack surface not covered by the existing CLI security program.  
**Gate:** Must be completed before v1.0 GA. Blocks the v1.0 tag.  
**Date drafted:** 2026-05-17

The CLI threat model (`THREAT-MODEL.md`) remains valid for the CLI crate. This document covers threats introduced by the HTTP server.

---

## 1. Injection scanning middleware

| Check | Status |
|---|---|
| `src/security/injection.rs` is called in POST /v1/projects/{id}/memory before any DB write | ☐ |
| No client header or query parameter can bypass the scan | ☐ |
| 422 response returns `field` and `category` — never the raw regex | ☐ |
| OnceLock pattern compilation verified: no per-request recompilation | ☐ |
| All 8 default patterns have positive and negative unit tests | ☐ |
| Audit log (`tracing::warn!`) fires on every match | ☐ |

## 2. API key authentication

| Check | Status |
|---|---|
| API key stored as BLAKE3 hash — plaintext never persisted | ☐ |
| Key comparison uses constant-time equality (not `==` on strings) | ☐ |
| `sk-sp-` prefix format validated before any DB lookup | ☐ |
| Revoked/deleted keys rejected immediately (no cache window) | ☐ |
| Key scope enforced: project-scoped key cannot write to other projects | ☐ |

## 3. Multi-tenancy isolation

| Check | Status |
|---|---|
| Every DB query includes an `org_id` filter (no bare table scans) | ☐ |
| RLS enabled and tested: a valid key from org A cannot read org B's entries | ☐ |
| Integration test: cross-org read attempt returns 403, not 404 or 200 | ☐ |

## 4. Input validation

| Check | Status |
|---|---|
| Title field: max 500 characters enforced at route handler | ☐ |
| Body field: max 50 000 characters enforced at route handler | ☐ |
| UUID path params validated (malformed UUID returns 400, not 500) | ☐ |
| All SQL uses parameterised queries — no string concatenation | ☐ |

## 5. SSE stream

| Check | Status |
|---|---|
| SSE connections require a valid API key on connection open | ☐ |
| Heartbeat tick re-validates key (revoked key disconnected within 60s) | ☐ |
| SSE events scoped to org — no cross-tenant event possible | ☐ |
| Integration test: revoke key, verify SSE connection closes within 60s | ☐ |

## 6. Dependencies

| Check | Status |
|---|---|
| `cargo audit` passes clean for spelunk-server crate | ☐ |
| `cargo deny` passes (licenses + sources) | ☐ |
| No yanked dependencies in Cargo.lock | ☐ |

## 7. Configuration and secrets

| Check | Status |
|---|---|
| Server refuses to start if JWT_SECRET is absent or < 32 bytes | ☐ |
| DATABASE_URL never logged (trace level or above) | ☐ |
| No secrets in default config files or committed .env files | ☐ |
| `.env*` excluded from any server-side file operations | ☐ |

## 8. Error responses

| Check | Status |
|---|---|
| 5xx responses do not leak stack traces or internal paths | ☐ |
| 422 injection responses reveal category, not pattern | ☐ |
| 401/403 responses consistent — cannot distinguish missing key from wrong key | ☐ |

---

## Running the checks

```bash
# From spelunk-server crate root (after workspace restructure)
cargo audit
cargo deny check
cargo clippy -p spelunk-server -- -W clippy::all -D warnings

# Integration tests (require running server + postgres)
cargo test -p spelunk-server --test integration

# Cross-tenant isolation test
cargo test -p spelunk-server cross_tenant

# SSE revocation test
cargo test -p spelunk-server sse_key_revocation
```

---

## Sign-off

All checks must be marked ✅ before the v1.0 tag is created. Add initials + date next to each check when complete.
