# spelunk style guide

How we write code and prose in this repository.

This document covers what tooling **cannot** check. Formatting is `cargo fmt`.
Lints are `cargo clippy`. If a rule can be mechanically enforced, it belongs in
`rustfmt.toml`, `clippy.toml`, or the workspace `[lints]` table, not here. When
you find yourself wanting to add a rule below, ask first whether a linter could
own it.

Everything here is descriptive of the codebase as it stands, except where a
section is marked **Changing**. Those are decisions we have made but not yet
finished applying; new code follows the new rule.

---

## Contents

1. [Comments](#1-comments)
2. [Documentation comments](#2-documentation-comments)
3. [Prose documentation](#3-prose-documentation)
4. [Errors](#4-errors)
5. [Naming](#5-naming)
6. [Modules and visibility](#6-modules-and-visibility)
7. [Types and signatures](#7-types-and-signatures)
8. [Tests](#8-tests)
9. [Panics](#9-panics)

---

## 1. Comments

**Audience: engineers maintaining this code.** They can already read Rust.

### Default to no comment

A comment must state something the code cannot. A hidden constraint, a
non-obvious invariant, the specific bug a branch guards against, or why one
approach was chosen over an obvious alternative. If a comment restates the next
line in English, delete it.

```rust
// Bad: the code already says this
// Check if project exists.
let existing = conn.query_row("SELECT id FROM projects WHERE slug = ?1", ...);

// Good: the ordering is load-bearing and invisible from the code
// Check batch size first so clients get a 413 even when no embedder is configured.
if batch.len() > MAX_BATCH { return Err(AppError::PayloadTooLarge); }
```

More examples of comments that earn their place, all from this codebase:

```rust
// Refuse to recursively delete through a symlink: a symlinked `.spelunk`
// (attacker-controlled or a poisoned registry row) could otherwise point
// `remove_dir_all` at an arbitrary directory outside the project root.

// Set once a chunk fails: the loop stops and the tombstone pass is skipped.

// Losers are deleted in id order; clearing their `superseded_by` first
// avoids a live FK reference regardless of that order.
```

Each states a fact you could not recover by reading the surrounding twenty
lines.

### Rename before you explain

If a comment exists only to say what a badly-named binding holds, the fix is the
name. Reach for a comment after naming has failed, not before.

### Be terse

State the constraint and stop. Cut the preamble, the restated history, and the
"here's why we're telling you this". A three-sentence comment that conveys one
fact is a candidate for a one-clause rewrite, not a trim.

### Mark nothing as TODO

The codebase currently has zero `TODO`, `FIXME`, `XXX`, or `HACK` comments, and
we intend to keep it that way. Unfinished work goes in an issue, where it is
visible, assignable, and closable. A `TODO` in source is an issue nobody filed.
A `TODO` is solutionizing before you fully understand the problem. It robs the
next engineer of agency to fix their problem. It also falls flat against YAGNI.


---

## 2. Documentation comments

**Audience: integrators consuming an API they cannot see the body of.**

Doc-comments (`///`, `//!`) are compiled into rustdoc and read away from the
source. That is what makes them different from a `//` comment, and it is also
the whole of their justification.

### Public API is documented

Every `pub` item that forms real external surface carries a `///`. State what it
does, what it returns, and any precondition or panic. Current coverage in
`spelunk-core` is around 90% of `pub fn` and 80% of `pub struct`; treat that as
the floor, not the target.

### Module docs on module roots

Every `mod.rs` and `lib.rs` should open with a `//!` explaining what the module
is for and how its pieces relate. This is the single highest-value doc-comment
in the tree and the one most often missing. The exception is test modules.

**Changing.** `spelunk-core/src/lib.rs` and `spelunk-server/src/lib.rs`, the two
crate roots, currently have none. Adding module docs to a file you are already
touching is always in scope.

### Never in test code

**No `///` or `//!` anywhere in `#[cfg(test)]` modules, on `#[test]` functions,
or in `crates/*/tests/`.**

Rustdoc is never generated for any of these. A doc-comment there is a category
error, not a style preference: it uses documentation syntax for something that
will never be documentation. Use `//` if a note is genuinely needed, but prefer
a descriptive test name and clear assertions over any comment at all.

```rust
// Bad
/// POST /v1/projects/{slug}/memory/search with no embedder should return 400.
#[test]
fn search_without_embedder() { ... }

// Good: the name is the documentation
#[test]
fn memory_search_without_an_embedder_returns_400() { ... }
```

**Changing.** 4,412 lines currently violate this, which is most of the test code
in the repository. `scripts/check-doc-comments.sh` reports them; it runs in CI in
report-only mode and does not fail a build. Enforcement is deferred until after
v1 so the conversion does not collide with release work. Until then: write new
test code to the rule, and convert what you touch. See [Tests](#8-tests) for how
test names carry the intent instead.

```bash
./scripts/check-doc-comments.sh --report
```

### Terse there too

"Read away from the code" is not a licence for prose. A doc-comment still states
the contract and stops. If explaining a function needs four paragraphs, the
function needs splitting or an ADR needs writing.

---

## 3. Prose documentation

Where a document lives determines who it is for. Write for that reader and no
other.

| Location               | Audience                       | Contains                                                                                                  |
| ---------------------- | ------------------------------ | --------------------------------------------------------------------------------------------------------- |
| `docs/*.md`            | **End users** of spelunk       | Install, commands, config, memory model, server setup. Task-oriented. Assumes no knowledge of the source. |
| `docs/adr/`            | **Maintainers and architects** | Why the architecture is what it is. Immutable once merged.                                                |
| `docs/architecture/`   | **Contributors**               | Living notes on how subsystems work. Mutable; keep current or delete.                                     |
| `CONTRIBUTING.md`      | **Contributors**               | Process: setup, PRs, commits, tests, ADRs.                                                                |
| `STYLE.md` (this file) | **Contributors**               | What good looks like, beyond what tooling checks.                                                         |
| `AGENTS.md`            | **AI agents**                  | Workflow, module map, commands. `CLAUDE.md` is a symlink to it.                                           |
| `README.md`            | **First-time visitors**        | The pitch and the first five minutes.                                                                     |

The most common failure here is writing contributor documentation into `docs/`,
where end users trip over it, or writing end-user documentation into `AGENTS.md`,
where nobody finds it.

### Cite the source

A doc that says "verified against `handlers.rs`" in its header stays accurate.
The docs in this repository that have rotted are, without exception, the ones
that did not name what they described. When you document behaviour, name the
file that implements it.

### Update docs in the same PR

If a change alters a command, flag, config key, endpoint, or on-disk format, the
doc change ships with it. A follow-up PR for docs is a doc that does not happen.

### ADRs

Significant design decisions get an ADR. The template:

```
# ADR-NNN: Title
**Date:**
**Deciders:**
## Context
## Decision
## Rationale            (table: Option | Considered | Rejected because)
## Consequences         (Easier / Harder / Revisit if)
## Security implications
```

An accepted ADR is immutable. When reality moves on, add a dated
`> **Superseded (YYYY-MM-DD):**` callout in place and write a new ADR. Do not
edit the original's reasoning: the record of what we believed and when is the
point.

---

## 4. Errors

### `anyhow` is the default, everywhere

Every function that can fail returns `anyhow::Result<T>`, in libraries as well
as binaries. This is a deliberate choice for an application, not an oversight:
callers overwhelmingly want to propagate, and a hand-rolled error enum per
module would be cost without benefit.

### `thiserror` only for errors somebody matches on

Define a typed error only when a caller several frames away must change its
behaviour based on the specific failure. Even then, the typed value is boxed
into `anyhow::Error` immediately and recovered with `downcast_ref` at the one
site that cares.

```rust
/// Kept distinct from a bare `anyhow::Error` so a caller (e.g. `spelunk-server`'s
/// HTTP handlers) can match on the failure kind instead of string-matching a message.
#[derive(Error, Debug)]
pub enum EmbedError { ... }
```

That doc-comment is the test: if you cannot name the caller that will match on
it, use `anyhow::bail!` instead. Use `thiserror` for these, never a hand-written
`Display` + `Error` impl.

**Changing.** `spelunk-server/src/db.rs` hand-rolls `DimensionMismatch` and
`ModelMismatch`; these should move to `thiserror`.

### Message style

**Lowercase, no trailing period, for every error message.**

`anyhow` composes messages with `: `, so every message is potentially a middle
link in a chain. A capitalised, full-stopped message reads correctly only when
it happens to land last.

```rust
// Good
.context("initialising registry schema")?
.with_context(|| format!("creating registry directory {}", parent.display()))?
anyhow::bail!("no spelunk project here; run `spelunk init` first")

// Bad
anyhow::bail!("A project cannot depend on itself.")
```

Prefer a gerund or noun phrase for `.context()` ("opening the keychain entry",
"fetching project id after register"). Terminal `bail!` messages may be a
sentence, still lowercase and unpunctuated at the end.

**Changing.** `.context()` strings already follow this consistently. `bail!` and
`anyhow!` messages are split roughly evenly between the two styles; new code
uses lowercase.

### Never leak internals to a remote caller

`spelunk-server` surfaces a message to an HTTP client only when the error is a
known, explicitly-typed, user-facing variant. Everything else becomes a generic
500 and is logged server-side. Match on the type; never sniff the message
string.

### Exit codes

The CLI has three deliberate failure tiers. They are not interchangeable:

| Command shape                          | Exit | Output                                                 |
| -------------------------------------- | ---- | ------------------------------------------------------ |
| Normal user command                    | 1    | `Error: {:?}` (full anyhow chain)                      |
| `spelunk plumbing <sub>`               | 2    | `error: {:#}` on stderr (single line, script-friendly) |
| `plumbing publish-notes --best-effort` | 0    | warning on stderr only                                 |

The third exists because it runs from a git pre-push hook and must never block a
push. If you add a command, pick the tier that matches its caller.

---

## 5. Naming

### Functions

- **No `get_` prefix.** Accessors are bare nouns: `dimension()`, `token_cap()`,
  `backend_kind()`.
- **Booleans read as predicates**: `is_auto`, `has_docstring`, `is_stale()`.
- **Async functions get no marker.** `embed`, `search`, `add`. The signature
  already says `async`.

### Constructors

`new()` for the primary constructor. When construction performs I/O it takes a
verb that says so: `Registry::open()` reads and migrates a database, and calling
it `new()` would hide that.

`from_*` converts an existing value into `Self`. `with_*` is essentially unused;
prefer a flexible parameter type over a constructor variant:

```rust
pub fn system(content: impl Into<String>) -> Self
```

**No hand-written builders.** The codebase has none. A struct literal or a
`new()` with `impl Into<T>` parameters has covered every case so far. If you
genuinely need a builder, say why in the PR.

### Tests

Test names are complete claims, in `snake_case`, with no `test_` prefix. They
read as given/when/then compressed into an identifier:

```
archived_rows_import_as_archived
a_removed_binary_stops_the_push
insert_embedding_joins_callers_transaction_and_rolls_back_with_it
budget_packs_by_priority_not_display_order
```

Long is fine. The name is the specification, and it is the only documentation a
test gets.

---

## 6. Modules and visibility

### Layout

`foo/mod.rs` for modules with children; a bare `foo.rs` for leaves. The codebase
does not use the `foo.rs` + sibling `foo/` form anywhere; do not introduce it.

A module file that grows past roughly 400 to 600 lines of logic gets split into
a directory of single-purpose files re-exported from `mod.rs`. This is a
guideline, not a gate; `handlers.rs` is a deliberate exception because every HTTP
route handler is arguably one unit.

### `mod.rs` is a facade

Declare implementation submodules privately, expose a curated surface:

```rust
pub mod backend;          // a real public sub-namespace
mod chunks;               // implementation detail
mod conventions;

pub use backend::{LocalMemoryBackend, MemoryBackend, NoteInput};
pub use db::Database;
```

`spelunk-embed/src/lib.rs` is the cleanest example in the tree: private modules,
explicit re-exports, nothing else public.

### Visibility is a statement of intent

| Marker       | Means                                                                |
| ------------ | -------------------------------------------------------------------- |
| `pub`        | Real external surface. Documented. Breaking it is a breaking change. |
| `pub(crate)` | Internal helper that must cross module boundaries within the crate.  |
| `pub(super)` | This submodule's API as seen by its parent, and nobody else.         |

`pub(super)` is used systematically across `cli/cmd/*`: each command file exposes
exactly one `pub(super) async fn`, callable only from its parent dispatcher. That
pattern is the default for a new subcommand.

Reach for the narrowest that compiles.

### Imports

Group `std`, then external crates, then `super::`/`crate::`, with a blank line
before the local group. Nest with braces rather than repeating a prefix.

```rust
use std::net::SocketAddr;

use anyhow::Result;
use axum::{Extension, Json, extract::{Path, Query, State}};

use super::{AppError, AppState};
```

**Glob imports (`use super::*`) are test-only.** All 69 occurrences in the tree
are inside `#[cfg(test)]`, and that is the rule, not a coincidence.

---

## 7. Types and signatures

- **Take `&str`, return `String`.** Take `&[T]`, return `Vec<T>`. Borrow at the
  boundary, own on the way out.
- **`impl Trait` in argument position** for a single obvious bound, almost always
  `impl Into<String>` or a closure. Not as a general substitute for a named type.
- **`Box<dyn Trait>` at backend boundaries.** `EmbeddingBackend`, `LlmBackend`,
  and `MemoryBackend` are all consumed dynamically. We prefer one code path over
  monomorphised copies; these are not hot loops.
- **`#[async_trait]` on every trait with async methods.** Uniformly, no
  exceptions.
- **Newtypes for wrapped shared state**, always a one-field tuple struct:
  `EmbedderSlot(Arc<RwLock<..>>)`, `RelayRegistry(Arc<Mutex<..>>)`.
- **`#[must_use]` on enums representing a degraded state** that a caller could
  silently drop. `WriterLock` and `LockAttempt` use it because an unobserved
  degradation is how silent data loss stays invisible. It is not needed on
  `Result`.

### Feature flags

Declare in the crate's `[features]` with a comment saying what the flag gates and
why. Put `#[cfg(feature = "...")]` directly on the gated item, at the `pub mod`
declaration site in the facade where possible. Never wrap a whole file body.

---

## 8. Tests

### Where they go

| Kind                                        | Location                                          |
| ------------------------------------------- | ------------------------------------------------- |
| Unit tests for a module's internals         | `#[cfg(test)] mod tests` in the same file         |
| A large inline suite that dwarfs the module | `#[cfg(test)] mod tests;` in a sibling `tests.rs` |
| Integration, CLI, and cross-crate behaviour | `crates/<crate>/tests/`                           |

Reach for the same file first. Move out only when the test module makes the
source file hard to navigate.

### Reusable fixtures

Shared helpers go in that crate's `tests/common/mod.rs`. Helpers that downstream
crates need go behind `spelunk-core`'s `test-support` feature, not copied.

Use `TempDir` for filesystem isolation and `:memory:` for hermetic SQLite. Use
`wiremock` to stub the server's HTTP surface rather than spawning a real one.
`assert_cmd` + `predicates` for anything that asserts on CLI output.

### `#[serial]` needs a reason

Process-global state is the only justification: `sqlite3_auto_extension`
registration, environment variables, or a mutated static. Say which, in a `//`
comment. Prefer a named key (`#[serial_test::serial(some_key)]`) so unrelated
serial tests still run in parallel with each other.

### `#[ignore]` needs a reason too

Always `#[ignore = "why"]`. Every ignored test in the tree currently explains
itself (network access, a model download, a live server); keep it that way.

### Assertions

Pass a message with the actual value to `assert!` when the condition alone will
not tell you what went wrong:

```rust
assert!(r.confidence >= 0.9, "confidence={}", r.confidence);
```

**Changing.** `pretty_assertions` is declared as a dev-dependency in all four
crates and imported in none. Either adopt it (`use pretty_assertions::assert_eq;`)
or drop it. Until that is decided, do not add new usage.

### What to test

Bug fixes need a test that fails without the fix. Features need the happy path
and the error paths. Property tests (`proptest`) suit invariants over generated
input; the chunker and token budget use them. Fuzz targets live in `fuzz/` for
anything parsing untrusted bytes.

---

## 9. Panics

Production code does not panic. There are no `panic!` calls outside tests, and
that is intentional.

`unwrap()` and `expect()` are acceptable in exactly three cases:

1. **Mutex poisoning.** A poisoned lock means another thread already panicked
   mid-mutation; crashing beats continuing with corrupt state.
2. **Compile-time-constant input.** A regex built from a string literal cannot
   fail at runtime. A failure is a programmer error caught by the first test run.
3. **An invariant an earlier branch established.** Use `expect()` with a message
   naming the invariant: `.expect("tls_enabled implies cert set")`.

Bare `unwrap()` is for cases where the reason is self-evident (a mutex guard, a
test). Anywhere else, `expect()` with a message that says why.

Everything else propagates with `?`.

---

## Applying this to a review

If you are reviewing a PR, these are the things a linter will not catch for you:

- Does each comment say something the code does not?
- Do doc-comments appear anywhere in test code?
- Are error messages lowercase and unpunctuated?
- Is anything `pub` that could be `pub(crate)` or `pub(super)`?
- Do test names state what they claim, without a `///` above them?
- Does an `unwrap()` fall into one of the three sanctioned cases?
- Did a change to a command, flag, or format update its documentation?
