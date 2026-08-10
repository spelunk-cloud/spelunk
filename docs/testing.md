# Testing Strategy

## Current state

Tests live under `crates/*/tests/` (integration-style, one binary per file)
plus `#[cfg(test)]` blocks colocated with the code they cover across all four
crates. The suite spans unit logic, real-SQLite integration, in-process
server-handler tests, CLI end-to-end tests, property-based tests, an upgrade
corpus of artifacts written by real released binaries, and a scheduled fuzzing
job.

The embedder stack is the native candle F2LLM path (`spelunk-embed`, gated by
the `embed-native` feature), not an external OpenAI-compatible endpoint. See
`CLAUDE.md` for the full inference-backend picture.

---

## Running the tests

```bash
cargo nextest run --lib --bins --tests --benches
cargo test --doc
```

This is what to run before pushing, and matches CI's own invocation
(`.github/workflows/ci.yml`) on each platform leg: `cargo nextest run` for
the workspace, plus `cargo test --doc` as a separate pass since nextest
does not run doctests. `--lib --bins --tests --benches` (not nextest's
default) keeps examples out of the regular test gate: several depend on
the native embedder and are meant to be run explicitly with the right
features, not swept in by a workspace-wide command that doesn't grant
them. Some CI legs add `--no-default-features` (see the workflow file for
exactly which); reach for that flag locally if you need to reproduce a
platform-specific failure.

For a tighter loop while iterating on one file:

```bash
# One test file
cargo nextest run -p spelunk-core --test integration_db

# Doctests: nextest does not run them, so they're a separate pass
cargo test --doc
```

CI (`.github/workflows/ci.yml`) is the source of truth for what actually
gates a merge: the test matrix, feature flags, and per-platform steps are
described there, not restated here where they can drift out of sync.

---

## Test layout

Each crate that has tests owns a `crates/<crate>/tests/` directory of
integration-style test binaries, plus `#[cfg(test)]` modules next to the code
under test in `src/`. Broad categories, not an exhaustive file list (a file
inventory is the kind of thing that goes stale the next time a test file is
added or renamed):

- **`crates/spelunk-core/tests/`**: chunker, embeddings, graph-edge, and
  summariser unit logic; adversarial/coverage-gap hardening for the chunker
  (`adversarial_chunker.rs`); real-SQLite integration tests against
  `Database` (CRUD, KNN search, graph edges, conventions, LIKE-metacharacter
  escaping); git-notes integration tests; a worktree-resolution integration
  test over a real `git worktree`; language-parsing coverage; property-based
  tests (`prop_*.rs`, using `proptest`).
- **`crates/spelunk-cli/tests/`**: CLI end-to-end tests that invoke the
  compiled `spelunk` binary via `assert_cmd`; plumbing-subcommand tests
  (`cat_chunks`, `graph_edges`, `knn`, `ls_files`, `parse_file`, `hash_file`)
  and porcelain/plumbing consistency checks; memory workflow tests (add,
  dedupe, reconcile, reindex, push/sync, cross-project visibility); auth,
  TLS-trust, and server-key resolution tests; git-hook and git-notes
  integration (pre-push publishing, hooks-path resolution, notes-carrier
  fallback/archive, `init`'s notes refspec); security/regression guards
  (secret-scanner bypass, harvest argument-injection, ANSI leaking onto
  non-tty stdout); and UX-guidance tests for index/search/inference-server
  messaging.
- **`crates/spelunk-server/tests/`**: Axum handler integration tests
  (in-process request/response, no socket bound); a real-TLS serve test
  (`tls_serve.rs`) that binds an actual loopback socket; and a real-socket
  plaintext CLI-to-server sync end-to-end test (`cli_sync_e2e.rs`) that
  drives the actual `spelunk memory push`/`spelunk sync` client code against
  a bound server instance.
- **`#[cfg(test)]` blocks in `src/`**: pure-logic unit tests colocated with
  the function they cover, across all crates (e.g. ANSI stripping, secret
  pattern detection, token estimation, memory dedupe logic).

Cross-crate HTTP boundaries (spelunk-server's own endpoints, sync/relay, auth)
are mocked with `wiremock` where a test needs an HTTP server without a real
network dependency.

---

## Upgrade corpus (the "DB museum")

Every other migration test in this repo builds an old database shape by hand.
That tests what we *believe* the old format was. The upgrade corpus tests what
it **is**: artifacts written by real, downloaded, released spelunk binaries,
checked in and opened with the current build on every relevant change.

```
crates/spelunk-cli/tests/upgrade_corpus.rs                the suite
crates/spelunk-cli/tests/fixtures/upgrade-corpus/         MANIFEST.json + gzipped wings
scripts/upgrade-corpus/                                   the generator
.github/workflows/upgrade-corpus.yml                      CI job
```

Six wings, all produced by actual releases, covering the pre-`user_version`
`index.db` whose version has to be inferred from its table shapes, a real
`FLOAT[768]` vector table, memory stores either side of the entity-id backfill,
a registry with a dependency link, and all three git-notes eras on one ref.

```sh
SPELUNK_SECRET_STORE=file cargo test -p spelunk-cli --test upgrade_corpus
```

It needs no network and no server: the fixtures are checked in, and the suite
expands each gzipped wing into a temp dir, since opening a database migrates it
and would otherwise destroy the fixture on first run. One test is `#[ignore]`d
because it needs a downloaded release binary in `SPELUNK_OLD_BINARY`; CI runs
that leg separately.

This suite is what enforces the on-disk half of the
[stability contract](stability.md#on-disk-formats). If you change a migration,
this is the test that tells you whether real field data survives it, and its
assertions are deliberately specific: they were built by injecting each
data-destroying regression into the real migration paths and confirming the
suite went red, so weakening one to make it pass is almost always the wrong
move.

Two things it documents that are easy to hit and hard to guess:

- an older binary opening a newer `index.db` re-stamps `PRAGMA user_version`
  **downwards**, which is not corruption. See
  [Downgrading, and why `user_version` can go backwards](stability.md#downgrading-and-why-user_version-can-go-backwards).
- adding a wing at each release, and the one assertion that is expected to flip
  when a release carrying the `memory.db` version guard is captured, are covered
  in [the corpus README](../scripts/upgrade-corpus/README.md).

Regenerating a wing is scripted but not reproducible byte for byte (wall-clock
timestamps, epoch-milli ids, absolute capture paths), so regenerate only the
wing you mean to change, with `generate.sh --only <wing-id>`.

---

## sqlite-vec in tests

`sqlite3_auto_extension` is process-global and must only be registered once
per process. `crates/spelunk-core/tests/common/mod.rs` guards this with a
`OnceLock`:

```rust
pub fn register_sqlite_vec() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

pub fn open_test_db() -> spelunk_core::storage::Database {
    register_sqlite_vec();
    spelunk_core::storage::Database::open(std::path::Path::new(":memory:"))
        .expect("failed to open in-memory database")
}
```

Tests that open a `Database` call `common::open_test_db()`. They are still
annotated `#[serial_test::serial]`, but see the next section for what that
annotation actually buys under the test runner CI uses.

`spelunk-cli` integration tests reach the same guard through
`tests/plumbing_helpers.rs::register_sqlite_vec`, alongside the other shared
fixtures in that module. They need it whenever they open a `rusqlite`
connection of their own against a DB a spawned `spelunk` binary wrote:
registration is per-process, so the child's does not carry over.

### `#[serial]` does not mean what it used to under nextest

CI runs tests with `cargo nextest run`, which gives every test its own OS
process. `serial_test`'s default lock (this workspace does
not enable its `file_locks` feature) is an in-process primitive: it only
serialises tests that share the same process's memory. Under nextest, no two
`#[test]` functions ever share a process, so `#[serial]` provides **no**
synchronisation there, in either direction:

- It doesn't protect `sqlite3_auto_extension`'s global registration, but that
  was never actually at risk from separate processes; each process gets its
  own address space and its own one-time registration.
- It also would **not** serialise a test's access to genuinely shared
  *external* state, if a test needed that: a file on disk, a bound TCP port,
  a git ref. Nextest's process-per-test model means such tests must
  synchronise through the external resource itself (a lock file, a
  retry/skip on port-in-use, an OS-level advisory lock), not through
  `#[serial]`.

`crates/spelunk-core/tests/integration_git_notes.rs` is the concrete example
already in this codebase: its concurrent-write tests are correct not because
of `#[serial]`, but because ADR-069 puts a real lock in the git common dir
that every writer, in every process, takes before a read-modify-write. That
lock is what makes the tests safe under nextest; `#[serial]` on those same
tests is redundant with it, not a substitute for it.

If you add a test that touches shared external state and see cross-run
flakiness, look for a lock on the resource itself before reaching for
`#[serial]`: it will not help.

---

## Ambient git config in tests

Tests that shell out to `git` in a temp repo do not start from a clean
slate: git also reads the contributor's real **global** and **system**
config. A repo's local config does not shadow a global value it never
sets, so an ambient `core.hooksPath` (husky, lefthook, the `pre-commit`
framework), `commit.gpgsign`, or `notes.rewriteRef` can make git behave
differently inside a test's throwaway repo than it does in CI, where no
ambient config exists.

Every test that spawns git must call an `isolate_git_config()` helper
first. It sets `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` to `/dev/null`
for the whole process, and clears `GIT_AUTHOR_*`/`GIT_COMMITTER_*`/`EMAIL`:
git resolves commit identity from those env vars before it consults config
at all, so they can override a test's own explicit `git config
user.name`/`user.email` unless cleared too. Guarded by `std::sync::Once` so
it is safe to call from every test. The isolation must be process-wide, not
scoped to one `Command`: a helper that only sets env on the `Command` it
builds itself never reaches git that the code under test spawns for itself.

For `spelunk-core` integration tests, prefer `common::git_command(cwd)` over
calling `isolate_git_config()` and `std::process::Command::new("git")`
separately: it bakes the isolation call into the `Command` it returns, so a
new test file cannot construct an un-isolated one by forgetting the setup
step.

`isolate_git_config`/`git_command` have exactly two definitions, both in
`spelunk-core`, one per side of the `src`/`tests` compilation boundary:

| Location | Covers |
|------|---------------|
| `crates/spelunk-core/src/test_support.rs` (module gated `#[cfg(any(test, feature = "test-support"))]`) | `spelunk-core`'s own `#[cfg(test)]` unit tests, and any downstream crate that enables the `test-support` feature |
| `crates/spelunk-core/tests/common/mod.rs` (also exports `git_command`) | `spelunk-core`'s `tests/` integration binaries |

`spelunk-cli` has no independent copy: `src/cli/cmd/test_support.rs` and
`tests/plumbing_helpers.rs` both `pub use spelunk_core::test_support::isolate_git_config`,
reaching it via a `spelunk-core = { path = "../spelunk-core", features =
["test-support"] }` dev-dependency (the same pattern
`config::secret_store::MemoryStore` already used for a src/tests-shared test
double before this).

Two, not one, because `spelunk-core`'s own `tests/` integration binaries link
the crate externally and can't reach a `#[cfg(test)]`-gated `src/` item
without a *self-referencing* dev-dependency
(`spelunk-core = { path = ".", features = ["test-support"] }` inside
`spelunk-core`'s own `Cargo.toml`). That was tried: it compiles and passes
against an isolated target dir, but this repo's pre-commit hook points
`CARGO_TARGET_DIR` at the shared `target/` used by every worktree, and
building against that shared dir (last built from a different `Cargo.lock`)
fails with `unresolved import spelunk_core::test_support`. The
`tests/common/mod.rs` duplicate is the real floor given that constraint, not
an oversight.

`scripts/check-git-isolation.sh` runs as the first step of CI's Check & Lint
job and fails the build if a test file spawns `git` (`Command::new("git")`,
however wrapped, whitespaced, or aliased) without wiring in one of the
above: a definition or call of `isolate_git_config`, a call to
`git_command`, or a `mod common;`/`mod plumbing_helpers;` import of the
fixture module. It's a grep-based heuristic, not a parser (see the script's
own header comment for exact scope and known blind spots, e.g. it can't
trace a spawn reached through a variable), but it catches the case that
used to slip through silently: a new test file that spawns git and forgets
isolation entirely.

---

## What is intentionally not tested

| Area | Reason |
|------|--------|
| Interactive `$EDITOR` (memory add without --body) | Requires a TTY; test manually |
| Real inference server output quality | Non-deterministic; use E2E heuristics at best |
| sqlite-vec KNN ranking precision | Depends on embedding geometry; covered by integration smoke tests |
| Concurrent SQLite writes under load | sqlite WAL handles this; benchmark separately if needed |
| PDF text extraction accuracy | Depends on PDF structure; smoke-test with a known fixture |

---

## CI matrix and platform notes

`.github/workflows/ci.yml` is the source of truth for the test matrix, the
per-platform feature flags, and the exact commands each job runs. The notes
below capture platform-specific *reasons* behind choices in that file, since
those are easy to lose ("why is this one command per step?") even when the
file itself stays accurate.

### Windows (`windows-latest`) caveats

- **One command per `run:` step.** GitHub wraps a Windows step in PowerShell,
  which aborts on a failing cmdlet but not on a failing native executable
  (`rustup`, `cargo`, etc.), since `$ErrorActionPreference='Stop'` +
  `exit $LASTEXITCODE` doesn't cover native exit codes. A `run:` block
  chaining two native commands can report success even when the first one
  failed. Steps in this job's Windows-inclusive matrix keep one command per
  step for that reason, e.g. the toolchain install is `Update stable
  toolchain` and `Set default toolchain` as separate steps rather than one
  `run: |` block. The same reasoning is why nextest and doctests run as
  separate steps rather than one chained command.

- **Build time.** Vendored OpenSSL (pulled in transitively by `native-tls`,
  via `hf-hub`/`reqwest` in the `embed-native` stack) compiles from C source.
  Strawberry Perl is pre-installed on `windows-latest` runners so the build
  succeeds, but it adds several minutes.

- **State-dir isolation.** E2E tests that set `.env("HOME", tmp)` to redirect
  spelunk's runtime state directory (`~/.local/state/spelunk/`) do not achieve
  full isolation on Windows because `dirs::home_dir()` uses the Windows Shell
  API (`SHGetKnownFolderPath`) rather than the `HOME` environment variable.
  Tests that need deterministic isolation should set `SPELUNK_STATE_DIR`
  directly instead of relying on `HOME`: it is a supported override of the
  entire state directory, read by the single resolver
  (`capability::spelunk_state_dir`) every reader and writer of runtime state
  goes through, so it bypasses the Windows `HOME` gap entirely.

- **`pid_is_alive` on Windows.** The Windows implementation uses
  `OpenProcess` + `GetExitCodeProcess` to check whether a process with a given
  PID is still running. This backs the `spelunk server status/stop` live-PID
  check on Windows.

- **Model download.** The `embed-native` feature bundles the candle F2LLM
  embedder; the model weights are fetched via `hf-hub` at runtime on first
  use, not at build time. Server tests run with the embedder slot disabled,
  so no model download happens during `cargo build` or the test run.

### Ubuntu (`ubuntu-latest`) caveats

- **`check` job disk pressure.** The `check`/lint job builds the full
  workspace (`--lib --bins --tests --benches --features rich-formats`, which
  pulls in the embedder dependency tree) and has intermittently exhausted the
  runner's free disk space. The job sets `CARGO_PROFILE_DEV_DEBUG: 0` to drop
  dev-profile debug info to reduce that pressure.

---

## Property-based tests and fuzzing

Both used to be "planned additions" here; both now exist and run in CI:

- **Property-based tests** (`proptest`) live in `crates/spelunk-core/tests/prop_*.rs`:
  `prop_chunker.rs`, `prop_token_budget.rs`, and `prop_embeddings.rs` (the
  `vec_to_blob`/`blob_to_vec` roundtrip plus blob-length invariants).
- **Fuzzing** targets live under `fuzz/fuzz_targets/` (parser, chunker, secret
  scanner, JSONL parsing, XML escaping, CLI history-entry parsing) and run on
  a schedule via `.github/workflows/fuzz.yml`, not on every push.

PageRank does not yet have property-based test coverage.
