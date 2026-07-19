# Internal task-tracker ID guard

**What:** a CI check — `.github/scripts/check-internal-ids.sh`, run by the
`internal-id-guard` job in `.github/workflows/ci.yml` — that rejects a
pull request whose introduced commits (message or added diff lines) contain
a reference to the internal task-tracker board. spelunk-oss is public; that
board is not.

## Why

The internal board's shorthand references have leaked into this repo's
history before: a `<project-slug>^<number>` reference (the board's own
citation shorthand), an opaque `task_<hex>` id, and the
`task/<persona>-<slug>-<UTC timestamp>` branch/worktree naming convention
agents use internally have each shown up in commit messages or file content
on `main`. None of those leaks exposed a secret — they're meaningless
outside the internal board — but they're noise in public history and a sign
the internal/public boundary isn't being enforced anywhere. Rewriting
existing history to remove them was judged not worth the disruption, so
those specific commits are left alone; this guard exists so the same class
of leak requires an explicit, deliberate override going forward instead of
landing silently.

## What it matches

Three patterns, `grep -E`, checked against **only the lines a change
introduces** — never a whole file, never existing history:

| Pattern | Matches | Example shape (not a real id) |
| --- | --- | --- |
| `\b(cloud-api\|spelunk-oss\|oss)\^[0-9]+\b` | a project-slug task-ref shorthand | `PROJECT^NNN` |
| `\btask_[0-9a-f]{16,}\b` | an opaque task id | `task_<16+ hex chars>` |
| `\btask/[a-z][a-z-]*-[0-9]{8}-[0-9]{4}\b` | the worktree/branch naming convention | `task/<persona>-<slug>-<UTC timestamp>` |

If you have a project convention this list should also cover, add a pattern
to the `PATTERNS` array in `check-internal-ids.sh` and a matching row here.

## Scope: going-forward only, by construction

The guard never scans a full file and never re-walks accepted history — only
`git diff --unified=0` **added** (`+`) lines within an explicit commit range,
plus the messages of the commits in that range. A pattern match sitting
untouched in a file, or in a commit already on `main` before the range
starts, is invisible to it. This isn't a filter bolted on top — the CI job
only ever hands it `<base>...<head>` for the current pull request, so there
is no code path that could accidentally rescan history.

This is a deliberate, discussed tradeoff, not an oversight: a leak found in
existing history is handled case by case (see the repo's change history for
prior instances), not by extending this guard's scope backwards.

## Where it runs

- **CI (enforced):** the `internal-id-guard` job in `.github/workflows/ci.yml`
  runs on every push and pull request against `main`. It first runs the
  guard's own fixture-based test suite
  (`.github/scripts/check-internal-ids.test.sh`), then runs the guard itself
  against the range introduced by the change (`base...head` for a PR, or
  `before...after` for a direct push).
- **Local (optional convenience):** the same script doubles as a
  `commit-msg` and/or `pre-commit` git hook — nothing in this repo installs
  git hooks automatically (see [`docs/commands.md`](../commands.md#spelunk-hooks)
  for the unrelated `spelunk hooks` product feature, which manages a
  *project's* hooks, not this repo's own). To opt in on your own clone:

  ```bash
  # reject a leaked ref in the commit message itself
  cat > .git/hooks/commit-msg <<'EOF'
  #!/usr/bin/env bash
  exec .github/scripts/check-internal-ids.sh --message "$1"
  EOF
  chmod +x .git/hooks/commit-msg

  # reject a leaked ref in the lines you're about to commit
  cat > .git/hooks/pre-commit <<'EOF'
  #!/usr/bin/env bash
  exec .github/scripts/check-internal-ids.sh --staged
  EOF
  chmod +x .git/hooks/pre-commit
  ```

  CI is the enforced gate either way; the local hooks just give faster
  feedback before a push.

## Known limitations (accepted)

This is a hygiene guard, not a security boundary — it isn't hardened against
someone deliberately evading it:

- **Split across lines.** `grep` matches per line; a ref broken across two
  lines (or two commits) is invisible to it.
- **Unicode lookalikes / zero-width characters.** A zero-width space inside
  the ref, or a Unicode caret lookalike instead of ASCII `^`, doesn't match.
- **A rewrite with low content-similarity that drags forward an
  already-accepted leak untouched.** `git diff` represents a rename/rewrite
  below its similarity threshold as a full delete+add, so an untouched
  historical line inside it reads as newly "added" — a false positive in the
  rare case this happens, not a missed leak.
- **A version-style ref glued to a project slug** — a caret-range dependency
  pin (no separator between the slug and the caret) reads as the project-slug
  pattern up through the first `.` and is flagged — a false positive, not a
  missed leak.

None of these have shown up in practice; noted here so a future change to
`PATTERNS` or the diff strategy doesn't need to rediscover them.

## Overriding

There is no bypass flag by design — a match means rewrite the offending line
so it doesn't cite the board (say what changed instead of citing the
tracker), then re-commit. If a specific line is a deliberate, reviewed
exception, get sign-off and adjust the pattern/exclusion list in
`check-internal-ids.sh` itself, in its own reviewed PR, rather than
suppressing a single occurrence inline.
