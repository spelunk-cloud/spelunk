# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| latest (`main`) | Yes |
| older releases | Best-effort patch backport for critical issues |

## Reporting a Vulnerability

**Please do not file public GitHub issues for security vulnerabilities.**

Report security issues privately via GitHub's built-in private vulnerability reporting:
**Security → Report a vulnerability** on the [spelunk repository](https://github.com/spelunk-cloud/spelunk/security/advisories/new).

### What to include

- Description of the vulnerability and its impact
- Steps to reproduce (spelunk version, OS, minimal reproduction)
- Any suggested fix or relevant code references

### Response SLA

| Severity | Acknowledgement | Patch target |
|----------|:-:|:-:|
| Critical (CVSS ≥ 9.0) | 48 hours | 7 days |
| High (CVSS 7.0–8.9) | 7 days | 30 days |
| Medium / Low | 7 days | Next minor release |

We will credit reporters in the release notes unless you prefer to remain anonymous.

## Scope

spelunk is primarily a **local single-user CLI tool**. By default it runs a
local `spelunk-server` bound to `127.0.0.1` for embeddings and inference; team
deployments may run a shared, authenticated `spelunk-server` reachable over the
network. The most relevant security concerns are:

- **Credential leakage** — secrets present in indexed source files being stored
  in the vector index, written through to git notes, or sent to an inference
  backend
- **Memory persistence** — memory entries are written through to
  `refs/notes/spelunk` by default (`store_in_git_notes`), so they travel with
  the repository on push/clone
- **Server exposure** — a `spelunk-server` bound beyond loopback, or run without
  an API key, exposes stored memory to anyone who can reach the port
- **Dependency vulnerabilities** — transitive Rust crate advisories
- **Data integrity** — corruption of the local SQLite index or memory database

For the default local configuration spelunk makes no outbound connections except
to `127.0.0.1` (a local server the user controls). When `server_url` points at a
remote instance, treat the network path and the server's authentication as
in-scope — see [`docs/security/THREAT-MODEL.md`](docs/security/THREAT-MODEL.md)
(Mode B) and [`docs/server-setup.md`](docs/server-setup.md) for TLS guidance.

## Security Controls

- Secret scanning runs before any chunk (docstring + content) is stored in the
  index, and again on LLM-generated summaries when they're produced
  (`src/indexer/secrets.rs`). This is best-effort defense-in-depth, not a
  security boundary — the boundary is that code never leaves the local
  machine unless a team `server_url` is explicitly configured
- `.env*`, `*.pem`, `*.key`, and similar sensitive file patterns are excluded
  from indexing unconditionally, matched case-insensitively
- All database writes use parameterised queries — no SQL string concatenation
- LLM prompts use XML delimiter isolation with angle-bracket escaping of all
  retrieved context
- `cargo audit` and `cargo deny` run in CI and block merges on unaddressed
  advisories

## Security Program

Full security program documentation: [`docs/security/SECURITY-PROGRAM.md`](docs/security/SECURITY-PROGRAM.md)  
Threat model: [`docs/security/THREAT-MODEL.md`](docs/security/THREAT-MODEL.md)
