---
name: verify
description: Verification gates for the spelunk repo: run before opening or updating any pull request. Covers the mandatory secret-store env var for cargo, formatting, clippy, tests, doctests, and the source-hygiene checks CI and review will otherwise catch. Use after making any change to this repository.
---

# Verify: spelunk

Run every gate below and fix until green. A gate you did not watch pass has not passed.

---

## 0. Every cargo command needs `SPELUNK_SECRET_STORE=file`

**Unconditional. Every cargo invocation, every time. `test`, `build`, `run`, `check`, `clippy`,
`fmt`, `nextest`.** Not a test-only concern: any command that links the crate can reach the
platform secret store.

```bash
SPELUNK_SECRET_STORE=file cargo test -p spelunk-cli
```

Without it, cargo reaches the real OS keyring and **blocks on a live interactive permission
prompt** on the developer's machine. It is not a test failure; the command simply hangs forever
waiting on a dialog someone has to physically dismiss.

Do not rely on inherited environment, a shell export from earlier in the session, or a
`.cargo/config.toml` default. A long-lived or stacked branch may be based on a commit that predates
the config fix, so "it's set in the repo now" is not something you can assume from the branch
you're on. Put it in the command.

> **If any cargo command runs longer than ~20s with no output, kill it immediately** and check for
> this before retrying. Waiting it out costs the whole session; a hung keyring prompt never
> resolves on its own.

## 1. Format

```bash
SPELUNK_SECRET_STORE=file cargo fmt --all -- --check
```

## 2. Clippy: zero warnings

```bash
SPELUNK_SECRET_STORE=file cargo clippy --all-targets --features rich-formats -- -D warnings
```

## 3. Build

```bash
SPELUNK_SECRET_STORE=file cargo build --all-targets --features rich-formats
```

## 4. Tests + doctests

```bash
SPELUNK_SECRET_STORE=file cargo nextest run
SPELUNK_SECRET_STORE=file cargo test --doc
```

Scope to the crate you touched while iterating (`-p spelunk-cli`), but run the full suite before
the PR.

## 5. Git isolation lint

```bash
scripts/check-git-isolation.sh
```

---

## Source hygiene: check your own diff

Everything below is checked against `git diff <base>...HEAD`, not the whole repo. You are
responsible for what your change introduced.

### 5.1 No external tracker references in shipped text

**This is a public repository.** Never write a reference to an internal tracker, planning board or
ticket id into shipped code, comments, test names, commit messages, docs or ADR text.

```bash
git diff <base>...HEAD | grep -nE '^\+.*(\^[0-9]+|#[0-9]+)'
```

Review every hit. A bare `#123` or `^123` in this repo reads to any future reader as *this repo's*
issue #123, which is a real, unrelated issue. That makes each occurrence a permanently wrong
pointer, not merely internal noise.

Describe **what** changed and **why**: the invariant, the bug, the behaviour. Never **which
ticket** prompted it. Cross-references to real GitHub issues *in this repo* are fine; that is what
the grep is for: reading the hits, not deleting them blindly.

### 5.2 No em-dashes

```bash
git diff <base>...HEAD | grep -n '^+.*—'
```

A repo convention, and it covers **committed docs as well as code**: comments, doc-comments,
Markdown, all of it. Use a colon, comma, semicolon, or period, or restructure the sentence.

```rust
// Bad
/// `server_limits` mirrors `/v1/health`'s `limits` object — `None` when absent — a server
/// that pre-dates the field.

// Good
/// `server_limits` mirrors `/v1/health`'s `limits` object: `None` when absent means a server
/// that pre-dates the field.
```

### 5.3 Comments explain WHY, not WHAT

A comment earns its place by carrying something the code cannot: a hidden constraint, an
invariant, a workaround and the reason for it. A comment restating what the next line plainly does
is noise, so delete it. Trim these as a matter of course when you touch a file, not only when
someone flags them.

### 5.4 No doc-comment syntax in tests

```bash
git diff <base>...HEAD -- '*/tests/*' 'tests/*' | grep -nE '^\+\s*(///|//!)'
```

No rustdoc is generated for tests, so `///` and `//!` there are dead weight. Use plain `//`, or
delete and let the test name carry it.

### 5.5 Content assets are not dead code

Never delete a committed image, video or other binary asset because nothing in the code references
it. Retention can be an intentional content, brand or archival decision that a reference grep
cannot see. Fix the stale doc text, leave the asset, and raise the deletion separately.

---

## Report

State each gate and its result. If you claim green, you ran it.
