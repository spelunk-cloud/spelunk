#!/usr/bin/env bash
# .github/scripts/check-internal-ids.sh — reject internal-only task-tracker
# references from landing in this public repo.
#
# spelunk-oss is public; the internal task-tracker board is not. Its
# shorthand references (a project-slug `^` number, an opaque `task_<hex>`
# id, and the `task/<persona>-<slug>-<UTC timestamp>` worktree/branch naming
# convention) have leaked into commit messages and file content before. This
# guard rejects any commit that introduces one, going forward.
#
# It is deliberately NOT a retroactive audit: it only ever looks at lines a
# commit *adds* (via `git diff`, never a full-file grep) and at commits
# within an explicit range, so history that already contains a leak is never
# rescanned or flagged. See docs/security/internal-id-guard.md.
#
# Usage:
#   check-internal-ids.sh --message <file>       # scan a commit message file
#                                                 #   (commit-msg hook)
#   check-internal-ids.sh --staged               # scan the staged diff
#                                                 #   (pre-commit hook)
#   check-internal-ids.sh --range <rev>..<rev>   # scan added lines + commit
#                                                 #   messages for a range
#                                                 #   (CI)
#
# Exits 0 if clean, 1 with the offending line(s) on stderr otherwise, 2 on
# misuse.

set -euo pipefail

# ---------------------------------------------------------------------------
# Patterns. Keep in sync with docs/security/internal-id-guard.md.
# Each is `grep -E`, so keep them portable (BSD grep on macOS, GNU grep in
# CI) — no PCRE-only constructs.
# ---------------------------------------------------------------------------
PATTERNS=(
  '\b(cloud-api|spelunk-oss|oss)\^[0-9]+\b' # project-slug task ref, e.g. a "<slug>^<number>" shorthand
  '\btask_[0-9a-f]{16,}\b'                  # opaque task id
  '\btask/[a-z][a-z-]*-[0-9]{8}-[0-9]{4}\b' # task/<persona>-<slug>-<UTC timestamp> branch/worktree name
)

# The guard's own script + test fixtures necessarily contain placeholder
# text shaped like the patterns above (to prove the patterns work) or
# describe them in prose. Neither is a real leak, so both are excluded from
# every diff this script scans — everything else in the repo stays covered,
# including this exclusion list itself.
EXCLUDE_PATHSPEC=(
  ':(exclude).github/scripts/check-internal-ids.sh'
  ':(exclude).github/scripts/check-internal-ids.test.sh'
)

fail=0

# check_text LABEL TEXT — scans (possibly multi-line) TEXT for every
# pattern, printing and flagging each matching line under LABEL.
check_text() {
  local label="$1" text="$2"
  local pattern matches line
  for pattern in "${PATTERNS[@]}"; do
    matches="$(printf '%s\n' "$text" | grep -E "$pattern" || true)"
    if [ -n "$matches" ]; then
      while IFS= read -r line; do
        if [ -n "$line" ]; then
          echo "internal-id-guard: $label matches an internal reference pattern:" >&2
          echo "    $line" >&2
          fail=1
        fi
      done <<<"$matches"
    fi
  done
  # Explicit, unconditional success: under `set -e` a bare `[ ... ]` as the
  # last-executed statement would otherwise make this function's (and thus
  # any caller-statement's) exit status reflect "no match found" as failure,
  # aborting the whole script even on clean input.
  return 0
}

# scan_added_lines DIFF — scans only '+' lines of a `git diff --unified=0`
# style patch (never removed or context lines), skipping the '+++' file
# header. This is what keeps the guard going-forward-only: a line that was
# already in the repo, or one being deleted, is never inspected.
scan_added_lines() {
  local diff="$1" added
  added="$(printf '%s\n' "$diff" | grep -E '^\+' | grep -Ev '^\+\+\+' | sed -E 's/^\+//' || true)"
  if [ -n "$added" ]; then
    check_text "added line" "$added"
  fi
  return 0 # see check_text's comment on why this is explicit
}

mode="${1:-}"
case "$mode" in
  --message)
    file="${2:?usage: check-internal-ids.sh --message <file>}"
    check_text "commit message" "$(cat "$file")"
    ;;
  --staged)
    diff="$(git diff --cached --unified=0 -- . "${EXCLUDE_PATHSPEC[@]}")"
    scan_added_lines "$diff"
    ;;
  --range)
    range="${2:?usage: check-internal-ids.sh --range <rev>..<rev> or <rev>...<rev>}"
    while IFS= read -r sha; do
      [ -z "$sha" ] && continue
      check_text "commit $sha message" "$(git log -1 --format=%B "$sha")"
    done < <(git rev-list "$range") || true # empty range: 0 iterations, `read` fails once
    diff="$(git diff --unified=0 "$range" -- . "${EXCLUDE_PATHSPEC[@]}")"
    scan_added_lines "$diff"
    ;;
  *)
    echo "usage: check-internal-ids.sh --message <file> | --staged | --range <rev>..<rev>" >&2
    exit 2
    ;;
esac

if [ "$fail" -ne 0 ]; then
  echo "" >&2
  echo "internal-id-guard: rejected — internal task-tracker references must not" >&2
  echo "ship in spelunk-oss (public repo). See docs/security/internal-id-guard.md" >&2
  echo "for what matched and why, and how to get an explicit override." >&2
  exit 1
fi

exit 0
