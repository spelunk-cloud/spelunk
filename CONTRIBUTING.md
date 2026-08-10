# Contributing to spelunk

Thanks for your interest in contributing. spelunk is a local-first code
intelligence CLI (plus an optional self-hosted server), and contributions of
all kinds are welcome: bug reports, docs fixes, features, and performance work.

## Getting set up

```bash
git clone https://github.com/spelunk-cloud/spelunk.git
cd spelunk
cargo build            # builds all four workspace crates
cargo test             # runs the full test suite
```

The Rust toolchain is pinned to `stable` via `rust-toolchain.toml` — rustup
picks it up automatically. Platform-specific notes (Metal on macOS, Linux
containers, Windows) are in [docs/building.md](docs/building.md).

## Before you open a PR

CI enforces formatting, lints, and tests. Run the same checks locally:

```bash
cargo fmt --all -- --check
cargo clippy --lib --bins --tests --benches --features rich-formats -- -D warnings
cargo test
```

Guidelines:

- **Keep PRs focused.** One change per PR; unrelated cleanups belong in their
  own PR.
- **Add tests.** Bug fixes need a test that fails without the fix; features
  need coverage for the happy path and the error paths.
- **Update docs in the same PR.** If a change alters commands, flags, config
  keys, or server endpoints, update the relevant page under `docs/` (and
  `docs/openapi.json` for server API changes).
- **Commit messages** follow the conventional style used in the log:
  `fix(index): …`, `feat(memory): …`, `docs: …`, `ci(tooling): …`.
- **Don't break stable surfaces.** CLI flags, plumbing JSONL output, exit
  codes, config keys, on-disk formats, and the server `/v1` API are
  compatibility surfaces. Additive changes are fine; breaking changes need
  discussion in an issue first.

## Architectural changes

Significant design decisions go through an ADR (architecture decision record)
in [docs/adr/](docs/adr/). Open an issue describing the problem first; an
accepted ADR is immutable once merged, so the discussion happens before, not
after. Living architecture notes go in `docs/architecture/`.

## Working on spelunk with an AI agent

This repository is itself indexed with spelunk, and [CLAUDE.md](CLAUDE.md)
documents the agent workflow (search before reading, store decisions in
`spelunk memory`). If you contribute using a coding agent, pointing it at
CLAUDE.md will make it noticeably more effective — and PRs remain judged on
the same bar either way: focused, tested, documented.

## Reporting bugs

Use the bug report template — it asks for `spelunk --version`, your platform,
install method, and `spelunk status` / `spelunk check` output, which is
usually the difference between a same-day fix and a week of back-and-forth.
For anything security-sensitive, **do not open a public issue** — see
[SECURITY.md](SECURITY.md) for private reporting.

## License

spelunk is [MIT](LICENSE) licensed. By contributing, you agree that your
contributions are licensed under the same terms (inbound = outbound). The
bundled embedding model has its own attribution — see
[docs/model-attribution.md](docs/model-attribution.md).
