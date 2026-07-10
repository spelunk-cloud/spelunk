# ADR-056: OSS Server Tenancy Model (Single Trust Domain)

**Date:** 2026-07-02
**Deciders:** founder (Johan), architect

> **Clarification (2026-07-10, spelunk-oss^122):** The Context below refers to
> `--host 0.0.0.0` with a shared key as the then-documented multi-developer
> setup. That describes the pre-hardening state the security review examined, not
> a supported posture today. The shipped binary refuses **any** non-loopback
> plaintext bind unconditionally, keyless *and* keyed alike, with no override,
> exactly as this ADR's own Decision and Consequences already require
> (`check_bind_safety` in `crates/spelunk-server/src/main.rs`). A shared
> deployment binds loopback and terminates TLS in an operator-owned front proxy
> ([ADR-058](058-team-server-bare-metal-deployment.md)). The authoritative
> operator-facing wording lives in [`server.md`](../server.md) and
> [`THREAT-MODEL.md`](../security/THREAT-MODEL.md). This note clarifies the
> historical Context; it does not change the decision.

## Context

`spelunk-server` is an HTTP listener (axum) that can hold a team's memory when a
developer sets an explicit `server_url` (the team-memory tier described in
[ADR-004](004-unified-memory-storage.md)). Its authentication is a single shared
bearer key: `ApiKeyAuth` (`crates/spelunk-server/src/auth.rs`) answers exactly
one question, "does the presented bearer match the configured
`SPELUNK_SERVER_KEY`?". When no key is configured the server accepts all
requests (intended for a local loopback bind).

The `Principal` produced by a successful check carries an identity token but no
project scope, and no request handler consults it for authorization. Every
`/v1/projects/{project_id}/...` handler scopes its database work solely by the
caller-supplied `project_id` slug in the path. Concretely, on a server:

- `GET /v1/projects` (`handlers.rs`) enumerates every project slug on the
  instance.
- `POST /v1/projects/{id}/memory` auto-creates a project on first write.
- Read, search, archive, supersede, and `DELETE /v1/projects/{id}/memory/{id}`
  (`db.rs`) all act on whatever slug the caller puts in the path.

A pre-v1.0 server security review flagged this as a cross-project access issue:
on a shared deployment (`--host 0.0.0.0` with one shared key, which is the
documented multi-developer setup), any holder of the key can enumerate all
projects and then read, modify, or permanently delete the memory of any project
on that instance simply by changing the slug in the request path.

This forces a decision that the code has never stated explicitly: **what is the
OSS server's tenancy model?** The relevant forces:

- The OSS server is a single-file SQLite service with no notion of organisations,
  users, or roles. There is no identity model to hang a per-project access
  control list on without introducing one.
- spelunk is positioned as a local-first tool with an optional shared team
  memory server ([ADR-001](001-scope-boundaries.md),
  [ADR-004](004-unified-memory-storage.md)). The shared server serves a single
  team that already shares trust in its codebase and its decisions.
- Building per-project or per-principal authorization (scoped keys plus an ACL,
  enforced on every handler) is a new authentication subsystem, not a small
  patch. It is a large amount of surface to design, implement, and test.
- The concrete danger today is not that the model is permissive; it is that the
  model is **undocumented**, so an operator can stand up a shared instance
  without realising every keyholder is a full administrator of every project on
  it.

## Decision

**A spelunk-server instance is a single trust domain, and its shared key is the
tenancy boundary.**

- Holding a server's key grants full participation in every project on that
  instance: list, read, search, write, supersede, archive, and delete. The
  server does not implement per-project or per-principal authorization, and
  `GET /v1/projects` enumerating all projects is intended behaviour.
- Isolation between projects or teams is achieved by running **separate server
  instances**, each with its own key and its own database. Two groups that must
  not see each other's memory run two servers.
- The no-key loopback configuration (a developer's local machine) is unchanged:
  it remains open on loopback only and is not a shared deployment.

This is a decision about the OSS server only. Fine-grained authorization within a
single instance is out of scope for the OSS server and would require a future
ADR that introduces an identity and scoping model.

## Rationale

| Option | Considered | Rejected because |
|---|---|---|
| **Single trust domain (chosen)** | ✅ | Matches how a shared team server is actually used (one team, shared trust), needs no identity model, ships now, and gives operators a clear mental model. Closes the real risk by making the boundary explicit. |
| Per-project scoped keys + ACL enforced on every handler | ✅ | A new authentication subsystem with no existing identity model in the SQLite server to build on. Large design/implementation/test surface. The isolation it provides is achieved more simply by running separate instances. |
| Leave the behaviour as-is and undocumented | ✅ | This is the status quo the review objected to. The failure mode is an operator who does not know that every keyholder is a full administrator. Silence is the vulnerability. |

The chosen model also keeps the server honest about what it is: a shared cache
and inference/memory layer for one team, not a multi-tenant platform. The
project-slug in the path is an addressing convenience, not a security boundary,
and this ADR says so out loud.

## Consequences

- **Easier:** v1.0 ships without an authorization subsystem; operators get a
  simple, correct mental model ("one server, one trust domain"); the code stays
  small.
- **Harder / out of scope:** there is no isolation between projects on a single
  instance. Running one instance for multiple teams that must not see each
  other's memory is explicitly unsupported. Anyone needing that runs separate
  instances.
- **Guardrails required (tracked as implementation work, not part of this ADR):**
  - The server emits a prominent startup warning when it is bound to a
    non-loopback interface **and** a key is set, stating that every keyholder can
    read, modify, and delete all projects on the instance and that separate
    instances are the way to isolate.
  - The deployment documentation and the fleet-management example state the
    single-trust-domain model plainly, and `GET /v1/projects` is documented as
    enumerating all projects by design.
  - The server serves plaintext HTTP only on a loopback bind. It refuses,
    unconditionally, to start on a non-loopback plaintext bind, whether that
    bind is keyless (an open, unauthenticated server) or keyed (the bearer key
    would cross the network in cleartext); the refusal names the interface and
    points at the loopback-only. There is no override. 
    The shared key never crosses the network in cleartext. `/v1/health` is an
    unauthenticated endpoint (no bearer required or sent).
- **Revisit if:** a genuine need appears to host mutually distrusting groups on
  one instance. That would be a new ADR introducing scoped keys and an ACL, and
  it would supersede this one.

## Security implications

This ADR defines a trust boundary that was previously implicit, which is itself
the primary mitigation. The consequences for the threat model:

- The shared server key is a bearer credential that grants full access to every
  project on the instance. It must be treated as a high-value secret:
  transmitted only over a secure transport, stored with restrictive
  permissions, and rotated on exposure. Because the key rides on the transport,
  the server restricts plaintext HTTP to loopback and unconditionally requires
  TLS for any non-loopback deployment, with
  no override. Key-comparison hardening
  (constant-time comparison) is tracked separately as part of the pre-v1.0
  server security review.
- The cross-project access that the review identified is reclassified from a
  vulnerability to documented, intended behaviour under this model. It is not a
  defect to be fixed with an ACL; it is the boundary the operator opts into by
  sharing one key.
- `docs/security/SECURITY-PROGRAM.md` and `docs/security/THREAT-MODEL.md` are
  updated to bring network exposure, authentication, and multi-user access into
  scope for the server and to record this single-trust-domain boundary. The
  server-audit checklist items that assume per-project or per-principal scoping
  are reframed as not applicable by design under this ADR, rather than as
  unmet requirements.
