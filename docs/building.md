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

### Git hooks

The repo ships a pre-commit hook (`cargo fmt --check` + `cargo clippy`) under
`.githooks/`. Git does not run hooks from a tracked directory on its own, so
point it there once per clone:

```bash
git config core.hooksPath .githooks
```

This also applies to any `git worktree add` checkout of this repo, since
worktrees share the parent repo's hooks path.

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
| `embed-native` | yes | Bundle the F2LLM-v2-330M native embedder via candle (CPU). Disable to build a server that relies on an external OpenAI-compatible embedding endpoint. |
| `metal` | no | Enable Metal GPU acceleration on macOS. Requires the `embed-native` feature. Add when building the macOS release binary for best performance. |

Enable non-default features with `--features`:

```bash
# macOS release build with Metal GPU acceleration
cargo build --release -p spelunk-server --features metal

# Server without the bundled embedder (external endpoint required)
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

## Running tests

```bash
cargo test
```

## Security audit

Requires [cargo-audit](https://crates.io/crates/cargo-audit):

```bash
cargo install cargo-audit
cargo audit
```

## Notes

- The `sqlite-vec` extension is bundled at compile time — no system SQLite extension needed.
- Tree-sitter grammars are compiled as part of the build. If you bump the `tree-sitter` core
  version, check that all `tree-sitter-*` grammar crates are compatible (see `Cargo.toml`).
- Release builds enable LTO and `codegen-units = 1` for a smaller, faster binary.
  Expect a longer compile on first release build.
- Shared dependency versions are declared in the workspace root `Cargo.toml` under
  `[workspace.dependencies]`. Bump versions there, not in each crate's `Cargo.toml`.
