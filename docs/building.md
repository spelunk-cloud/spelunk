# Building from Source

Most users should install from a prebuilt binary — see [Getting Started](getting-started.md).
Build from source if you want to modify spelunk, run the latest unreleased code, or
target a platform without a prebuilt release (Intel Macs included — no
`x86_64-apple-darwin` prebuilt is published).

## Prerequisites

### Rust

Install via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Rust 1.80 or later is required (spelunk uses the 2024 edition).

### No external inference server required

From v0.9.0, `spelunk-server` bundles a native embedder
(codefuse-ai/F2LLM-v2-330M, 896-dim, via candle). No LM Studio, Ollama, or
other external inference server is needed. The CLI auto-starts the server on
first use; model weights are downloaded once and cached under
`~/.local/share/spelunk/models/`.

If you want GPU acceleration on macOS, build `spelunk-server` with the `metal`
feature (see [Build feature flags](#build-feature-flags) below).

## Build

This is a Cargo workspace with three crates: `spelunk-core` (library),
`spelunk-cli` (`spelunk` binary), and `spelunk-server` (`spelunk-server` binary).
Build them all together:

```bash
git clone https://github.com/spelunk-cloud/spelunk
cd spelunk

# Debug build (faster compile, slower runtime)
cargo build

# Release build (optimised — use this for day-to-day use)
cargo build --release
```

This produces both binaries under `target/release/`. Copy them to your `$PATH`:

```bash
cp target/release/spelunk target/release/spelunk-server ~/.local/bin/
# or
sudo cp target/release/spelunk target/release/spelunk-server /usr/local/bin/
```

Verify:

```bash
spelunk --version
spelunk-server --version
```

### Building individual binaries

```bash
# CLI only
cargo build --release -p spelunk-cli

# Server only
cargo build --release -p spelunk-server
```

## Build feature flags

### spelunk-server features

| Feature | Default | Description |
|---|---|---|
| `embed-native` | yes | Bundle the F2LLM-v2-330M native embedder via candle (CPU). Disabling it builds a server with no embedding capability at all: embed endpoints return a permanent 400 (there is no external-endpoint fallback). |
| `metal` | no | Enable Metal GPU acceleration on macOS. Requires the `embed-native` feature. Add when building the macOS release binary for best performance. |

Enable non-default features with `--features`:

```bash
# macOS release build with Metal GPU acceleration
cargo build --release -p spelunk-server --features metal

# Server without the bundled embedder (no embedding capability at all)
cargo build --release -p spelunk-server --no-default-features
```

### spelunk-cli features

| Feature | Default | Description |
|---|---|---|
| `rich-formats` | no | Enable parsing of PDF, DOCX, and XLSX files during indexing (pulls in `lopdf`, `docx-rs`, and `calamine`). |

```bash
# CLI with rich document format support
cargo build --release -p spelunk-cli --features rich-formats
```

### spelunk-core features

| Feature | Default | Description |
|---|---|---|
| `rich-formats` | no | Same as above — `spelunk-cli/rich-formats` propagates to this crate automatically. |

## Running tests and lints

```bash
make check
```

`make check` is the one command to run before pushing. It runs the **Check & Lint**
and **Test** legs of CI, and CI runs the same make targets, so a green `make check`
and a green CI agree by construction rather than by convention.

Do not hand-assemble the equivalent cargo commands. Every gate has a detail that is
easy to get wrong: CI lints with `--features rich-formats` (a warning reachable only
under that feature passes a plain clippy run), it runs `cargo nextest run` rather than
`cargo test`, doctests need a separate `cargo test --doc` because nextest does not run
them, and the suite covers two feature configs.

Two traps to know about:

- **zsh and `PIPESTATUS`:** Never pipe a gate into `tail` or `head` to shorten output. zsh
  has no `PIPESTATUS` at all: that is a bash name. zsh's array is `pipestatus`, lowercase and
  1-indexed, so `${PIPESTATUS[0]}` and `${PIPESTATUS[1]}` are both empty, and a bare `$?`
  after the pipeline is `tail`'s status rather than the gate's. `make test | tail -20` hides
  a real failure either way. Run the target and read its true exit status.
- **Nextest and `#[serial]`:** `#[serial]` is a no-op under nextest (each test gets its own
  process). A test relying on it to serialise shared external state (a file, a port, a git
  ref) is unguarded in CI regardless of what it claims locally.

| Target | What it runs |
|---|---|
| `make check` | `make lint` then `make test`. |
| `make lint` | `cargo fmt --all -- --check`, clippy, check, and build, all with `rich-formats`. |
| `make test` | `cargo nextest run` and `cargo test --doc`, for default and `--no-default-features`. |
| `make fmt` | Reformat in place. |
| `make precommit` | fmt and clippy only. A fast subset for a git hook, not a substitute for `make check`. |

Run `make help` for the full list. `make test` needs
[cargo-nextest](https://nexte.st) (`cargo install cargo-nextest --locked`); the target
says so rather than falling back to a different runner than CI uses.

The targets honour `CARGO_TARGET_DIR`, so a shared target directory works as usual.

### What `make check` does not cover

A green `make check` does **not** mean every CI job passes. These legs have no local
equivalent, or are deliberately left opt-in because they are slow or need extra tools:

| CI job | Local target |
|---|---|
| cargo-audit (Security) | `make audit` |
| cargo-deny (Security) | `make deny` |
| OpenAPI snapshot check | `make openapi-check`, and `make openapi` to regenerate |
| Workflow/Makefile drift guard | `make ci-drift` |
| Upgrade corpus: pinned-old-binary leg | none |
| Test (windows-latest) | none |
| Docker image build | none |
| Release script tests | none |
| Fuzz (weekly) | none |

Two PR-gating jobs are absent from that list because `make check` already covers them.
The Stability contract job re-runs `schema_contract_checker`, `plumbing_jsonl_contract`
and `plumbing_exit_codes` on their own so a contract break is a named failure rather
than one red line in a thousand-test run, but the full suite runs those same binaries.
The Upgrade corpus job's fixture leg is likewise an ordinary test. Only its second leg
is out of reach locally: it is `#[ignore]`d and needs a downloaded release binary in
`SPELUNK_OLD_BINARY`, which is why it has a row above.

For the legs with no local target, run CI on your branch. It does not need a pull
request:

```bash
gh workflow run ci.yml --ref "$(git branch --show-current)"
```

That is the only way to reach the Windows test leg while iterating, so use it for any
change with platform-specific risk.

## Security audit

Requires [cargo-audit](https://crates.io/crates/cargo-audit):

```bash
cargo install cargo-audit
make audit
```

## Notes

- The `sqlite-vec` extension is bundled at compile time — no system SQLite extension needed.
- Tree-sitter grammars are compiled as part of the build. If you bump the `tree-sitter` core
  version, check that all `tree-sitter-*` grammar crates are compatible (see `Cargo.toml`).
- Release builds enable LTO and `codegen-units = 1` for a smaller, faster binary.
  Expect a longer compile on first release build.
- Shared dependency versions are declared in the workspace root `Cargo.toml` under
  `[workspace.dependencies]`. Bump versions there, not in each crate's `Cargo.toml`.
