# spelunk Security Program

**Framework:** OWASP SAMM v2  
**Target maturity:** Level 1 across all 15 practices (pre-launch baseline)  
**Aspirational:** Level 2 in Secure Build and Security Testing by v1.0  
**Review cadence:** Each major release milestone

---

## Architecture in scope

spelunk ships in two operational modes, and the security program covers both:

- **Local CLI (default).** A single-user developer CLI that indexes source
  trees, stores chunks and memory notes in local SQLite, and reaches an
  inference backend for embeddings and LLM calls. From v0.9 the default backend
  is an auto-discovered `spelunk-server` bound to loopback (`127.0.0.1`); it is
  an inference backend only and never a memory store of record.
- **Optional shared `spelunk-server`.** An axum HTTP listener
  (`crates/spelunk-server/`) that holds a team's memory when a developer sets an
  explicit `server_url`. It exposes memory CRUD and semantic search over the
  network, authenticates callers with a single shared bearer key (`ApiKeyAuth`),
  and routes requests by a project-slug path segment.

Because the shared server exists, **network exposure, authentication, and
multi-user access control are IN scope** for this program. The server's tenancy
model is a deliberate design choice, not an omission: a server instance is a
**single trust domain**, and its shared key is the boundary (see
[ADR-056](../adr/056-oss-server-tenancy-model.md)). Every keyholder is a full
administrator of every project on that instance; isolation between teams is
achieved by running separate instances, each with its own key and database.

The relevant threats, across both modes:

| Threat                                   | Mode | Likelihood | Impact | Controls                                                      |
| ---------------------------------------- | ---- | ---------- | ------ | ------------------------------------------------------------- |
| Credential leakage into vector index     | CLI  | Medium     | High   | `secrets.rs` scanner; file exclusion in `ignore` traversal    |
| Prompt injection via indexed source code | CLI  | Low        | Medium | XML delimiters in `ask.rs`; angle-bracket escaping in context |
| Data integrity corruption (memory DB)    | CLI  | Low        | Medium | Atomic transactions in `storage/memory.rs`                    |
| Unauthenticated network access to server memory | Server | Medium | High | Bearer key (`ApiKeyAuth`); a non-loopback bind without a key is refused at startup |
| Bearer key disclosure / brute force on server | Server | Low | High | Key hashed with BLAKE3; per-request constant-time compare; shared key is a high-value secret (ADR-056) |
| Cross-project read/write on a shared server | Server | n/a | n/a | Intended under the single-trust-domain model (ADR-056), not a defect; isolate by running separate instances |
| Server denial of service (oversized / slow requests) | Server | Low | Medium | Request-body cap, per-route timeout, concurrency and rate limits |
| Dependency vulnerability                 | Both | Medium     | Medium | `cargo audit` + `cargo deny` in CI                            |
| Supply chain compromise                  | Both | Low        | High   | `cargo deny` license/source policy; `Cargo.lock` committed    |

Full threat model: [`docs/security/THREAT-MODEL.md`](THREAT-MODEL.md). The
server-specific pre-v1.0 checklist is
[`docs/security/V1-SERVER-AUDIT.md`](V1-SERVER-AUDIT.md).

---

## SAMM v2 Posture

### Current State (April 2026)

| Business Function  | Practice                    | Current Level | Target Level |
| ------------------ | --------------------------- | :-----------: | :----------: |
| **Governance**     | Strategy & Metrics          |       1       |      1       |
|                    | Policy & Compliance         |       0       |      1       |
|                    | Education & Guidance        |       1       |      1       |
| **Design**         | Threat Assessment           |       1       |      1       |
|                    | Security Requirements       |       0       |      1       |
|                    | Secure Architecture         |       1       |      1       |
| **Implementation** | Secure Build                |       2       |      2       |
|                    | Secure Deployment           |       1       |      1       |
|                    | Defect Management           |       1       |      1       |
| **Verification**   | Architecture Assessment     |       1       |      1       |
|                    | Requirements-driven Testing |       1       |      1       |
|                    | Security Testing            |       1       |      2       |
| **Operations**     | Incident Management         |       0       |      1       |
|                    | Environment Management      |       2       |      2       |
|                    | Operational Management      |       1       |      1       |

### Gaps to Close Before Launch

1. **Policy & Compliance L1** — Publish a `SECURITY.md` with responsible disclosure process and a brief secure coding policy reference in `CLAUDE.md`. Owned by: Docs Writer.
2. **Security Requirements L1** — Define a minimal security acceptance checklist for issues and PRs (secret handling, SQL parameterisation, input validation at boundaries). Owned by: Architect.
3. **Incident Management L1** — `SECURITY.md` must include a private vulnerability reporting contact and a defined response SLA (acknowledge within 7 days, patch within 30 for critical). Owned by: Docs Writer.

---

## Security Controls Inventory

### Build-time controls

| Control                          | Where                      | CI gate?           |
| -------------------------------- | -------------------------- | ------------------ |
| Secret scanning before indexing  | `src/indexer/secrets.rs`   | No (runtime)       |
| Dependency advisory scan         | `cargo audit`              | Yes — blocks merge |
| Dependency license/source policy | `cargo deny`               | Yes — blocks merge |
| Static analysis                  | `cargo clippy -D warnings` | Yes — blocks merge |
| Internal task-tracker ID leak guard | `.github/scripts/check-internal-ids.sh` | Yes — blocks merge |

### Design controls

| Control                                  | Where                             |
| ---------------------------------------- | --------------------------------- |
| Parameterised SQL (no string formatting) | All `storage/*.rs`                |
| XML delimiter isolation for LLM prompts  | `cli/cmd/ask.rs`                  |
| Angle-bracket escaping in RAG context    | `cli/cmd/ask.rs` (issue #137)    |
| Atomic transactions for memory state     | `storage/memory.rs`              |

### Server controls (shared `spelunk-server`)

| Control                                          | Where                                         |
| ------------------------------------------------ | --------------------------------------------- |
| Bearer-key auth; key hashed (BLAKE3), constant-time compare | `crates/spelunk-server/src/auth.rs`  |
| Refuse a non-loopback bind without a key         | `crates/spelunk-server/src/main.rs`           |
| Request-body cap, per-route timeout, concurrency and rate limits | `crates/spelunk-server/src/lib.rs`, `handlers.rs` |
| Title / body length caps and project-slug caps at the handler | `crates/spelunk-server/src/handlers.rs`  |
| Generic 5xx (no internal detail leak); safe error mapping | `crates/spelunk-server/src/lib.rs`, `db.rs` |

### Operational controls

| Control                                          | Where                      |
| ------------------------------------------------ | -------------------------- |
| `.env*`, `*.pem`, `*.key` excluded from indexing | `src/cli/cmd/index/mod.rs` |
| RUSTSEC advisory monitoring                      | `.cargo/audit.toml`, `deny.toml` |

---

## Secure Development Lifecycle Touchpoints

### Per-feature (every GitHub issue)

- Architect includes security acceptance criteria in the issue body
- Implementer runs `cargo audit` before every commit
- Test Engineer writes at least one adversarial/boundary test per security-sensitive path
- QA Reviewer checks for SQL string concatenation, unsanitised input, secret patterns

### Per-PR

- QA Reviewer runs the security checklist
- CI must pass: `cargo clippy`, `cargo audit`, `cargo deny`

### Per-release

- Full `cargo audit` clean (no unignored advisories)
- Re-run secret scanning patterns against the test fixture corpus
- Update `SAMM-POSTURE.md` with any practice level changes
- Check `SECURITY.md` contact details are still valid

---

## Responsible Disclosure

See `SECURITY.md` at the repo root.

---

## References

- [OWASP SAMM v2](https://owaspsamm.org/model/)
- [OWASP Top 10:2025 — Establishing a Modern AppSec Program](https://owasp.org/Top10/2025/0x03_2025-Establishing_a_Modern_Application_Security_Program/)
- [OWASP ASVS](https://owasp.org/www-project-application-security-verification-standard/) (L1 subset for the CLI; the HTTP server draws on the network-facing controls too)
- [`docs/security/THREAT-MODEL.md`](THREAT-MODEL.md)
- [`docs/security/V1-SERVER-AUDIT.md`](V1-SERVER-AUDIT.md)
- [ADR-056: OSS server tenancy model](../adr/056-oss-server-tenancy-model.md)
