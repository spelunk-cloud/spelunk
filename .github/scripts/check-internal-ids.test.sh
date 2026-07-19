#!/usr/bin/env bash
# .github/scripts/check-internal-ids.test.sh — fixture-based tests for
# check-internal-ids.sh (self-contained: no test framework, matches this
# repo's other zero-dependency CI scripts).
#
# Run: bash .github/scripts/check-internal-ids.test.sh
#
# Every fixture below uses placeholder ids that are obviously fake (e.g. an
# out-of-range `oss^999999`, an all-zero `task_...` hex string, a
# `task/engineer-faketest-...` branch name with a 9999-in-the-future
# timestamp) — never a real internal task-tracker reference. These fixtures
# live inside throwaway git repos created under a tmpdir for the duration of
# a single test and are never committed to *this* repo, but the literal
# strings still appear in this file's own source, which is why
# check-internal-ids.sh excludes this file (and itself) from the scans it
# runs against spelunk-oss's real history.

set -euo pipefail

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-internal-ids.sh"

# Deterministic, config-file-free identity for every synthetic commit —
# env vars only, so no test ever touches a git config file (local or
# global).
export GIT_AUTHOR_NAME="Test" GIT_AUTHOR_EMAIL="test@example.invalid"
export GIT_COMMITTER_NAME="Test" GIT_COMMITTER_EMAIL="test@example.invalid"

pass=0
fail=0

# ok DESCRIPTION — record a passing case.
ok() { pass=$((pass + 1)); echo "ok   - $1"; }
# bad DESCRIPTION DETAIL — record a failing case with why.
bad() {
  fail=$((fail + 1))
  echo "FAIL - $1"
  echo "       $2"
}

# new_repo — create and cd into a fresh throwaway repo, print its path.
new_repo() {
  local dir
  dir="$(mktemp -d)"
  git -C "$dir" init -q -b main
  echo "$dir"
}

# assert_status DESCRIPTION EXPECTED_STATUS ACTUAL_STATUS OUTPUT
assert_status() {
  local desc="$1" expected="$2" actual="$3" output="$4"
  if [ "$actual" = "$expected" ]; then
    ok "$desc"
  else
    bad "$desc" "expected exit $expected, got $actual. Output:
$output"
  fi
}

# assert_contains DESCRIPTION HAYSTACK NEEDLE
assert_contains() {
  local desc="$1" haystack="$2" needle="$3"
  if printf '%s' "$haystack" | grep -qF "$needle"; then
    ok "$desc"
  else
    bad "$desc" "expected output to contain '$needle'. Output:
$haystack"
  fi
}

# =============================================================================
# --message mode
# =============================================================================

run_message_case() {
  local msg="$1" file status output
  file="$(mktemp)"
  printf '%s\n' "$msg" >"$file"
  set +e
  output="$("$SCRIPT" --message "$file" 2>&1)"
  status=$?
  set -e
  rm -f "$file"
  printf '%s\x1f%s' "$status" "$output"
}

result="$(run_message_case "docs: expand the getting-started guide")"
status="${result%%$'\x1f'*}"
assert_status "clean commit message passes --message" 0 "$status" "${result#*$'\x1f'}"

result="$(run_message_case "fix: address feedback from spelunk-oss^999999")"
status="${result%%$'\x1f'*}"; output="${result#*$'\x1f'}"
assert_status "project-slug ref in message fails --message" 1 "$status" "$output"
assert_contains "failing message names the offending line" "$output" "spelunk-oss^999999"

result="$(run_message_case "fix: closes task_00112233445566778899")"
status="${result%%$'\x1f'*}"; output="${result#*$'\x1f'}"
assert_status "opaque task id in message fails --message" 1 "$status" "$output"

result="$(run_message_case "chore: picked up in task/engineer-faketest-99990101-0000")"
status="${result%%$'\x1f'*}"; output="${result#*$'\x1f'}"
assert_status "worktree-branch ref in message fails --message" 1 "$status" "$output"

result="$(run_message_case "fix: our biggest boss^2 enemy in the game")"
status="${result%%$'\x1f'*}"
assert_status "near-miss text (no word boundary) does not false-positive" 0 "$status" "${result#*$'\x1f'}"

# =============================================================================
# --staged mode
# =============================================================================

repo="$(new_repo)"
(
  cd "$repo"
  printf 'hello world\n' >notes.txt
  git add notes.txt
  set +e
  output="$("$SCRIPT" --staged 2>&1)"
  status=$?
  set -e
  printf '%s\x1f%s' "$status" "$output"
) >/tmp/case_out.$$
result="$(cat /tmp/case_out.$$)"; rm -f /tmp/case_out.$$
status="${result%%$'\x1f'*}"
assert_status "clean staged addition passes --staged" 0 "$status" "${result#*$'\x1f'}"
rm -rf "$repo"

repo="$(new_repo)"
(
  cd "$repo"
  printf 'context: relates to cloud-api^999999 (fake, for the test only)\n' >notes.txt
  git add notes.txt
  set +e
  output="$("$SCRIPT" --staged 2>&1)"
  status=$?
  set -e
  printf '%s\x1f%s' "$status" "$output"
) >/tmp/case_out.$$
result="$(cat /tmp/case_out.$$)"; rm -f /tmp/case_out.$$
status="${result%%$'\x1f'*}"; output="${result#*$'\x1f'}"
assert_status "leaked ref in staged addition fails --staged" 1 "$status" "$output"
assert_contains "failing --staged output names the offending line" "$output" "cloud-api^999999"
rm -rf "$repo"

repo="$(new_repo)"
(
  cd "$repo"
  # A short digit-only run after "task_" is well under the 16-char floor —
  # ordinary variable-ish text, not a task id, must never trip the guard.
  printf 'task_42 = compute_batch_size()\n' >notes.txt
  git add notes.txt
  set +e
  output="$("$SCRIPT" --staged 2>&1)"
  status=$?
  set -e
  printf '%s\x1f%s' "$status" "$output"
) >/tmp/case_out.$$
result="$(cat /tmp/case_out.$$)"; rm -f /tmp/case_out.$$
status="${result%%$'\x1f'*}"
assert_status "short task_-prefixed identifier does not false-positive" 0 "$status" "${result#*$'\x1f'}"
rm -rf "$repo"

repo="$(new_repo)"
(
  cd "$repo"
  # Everything staged is under the guard's own excluded paths, so the diff
  # is empty after exclusion — the exact shape of the bug this regression
  # test exists for: an empty-after-exclusion diff must still exit 0, not
  # abort on the `[ -n "$added" ]` test having nothing to report.
  mkdir -p .github/scripts
  printf 'fixture: task_aabbccddeeff00112233\n' >.github/scripts/check-internal-ids.sh
  printf 'fixture: task_aabbccddeeff00112233\n' >.github/scripts/check-internal-ids.test.sh
  git add .github/scripts/check-internal-ids.sh .github/scripts/check-internal-ids.test.sh
  set +e
  output="$("$SCRIPT" --staged 2>&1)"
  status=$?
  set -e
  printf '%s\x1f%s' "$status" "$output"
) >/tmp/case_out.$$
result="$(cat /tmp/case_out.$$)"; rm -f /tmp/case_out.$$
status="${result%%$'\x1f'*}"
assert_status "staging only the guard's own excluded paths passes --staged" 0 "$status" "${result#*$'\x1f'}"
rm -rf "$repo"

# =============================================================================
# --range mode: the going-forward-only guarantee
# =============================================================================

repo="$(new_repo)"
(
  cd "$repo"
  # Seed "accepted history": a commit that already contains a leaked-shaped
  # reference, standing in for real history this guard intentionally never
  # rewrites or rescans.
  printf 'legacy note: see spelunk-oss^999998 (fake, pre-existing "history")\n' >history.txt
  git add history.txt
  git commit -q -m "seed: pre-existing history"
  base="$(git rev-parse HEAD)"

  # A later, unrelated commit that touches nothing referencing the pattern.
  printf 'unrelated change\n' >other.txt
  git add other.txt
  git commit -q -m "chore: unrelated follow-up"
  head="$(git rev-parse HEAD)"

  set +e
  output="$("$SCRIPT" --range "$base..$head" 2>&1)"
  status=$?
  set -e
  printf '%s\x1f%s' "$status" "$output"
) >/tmp/case_out.$$
result="$(cat /tmp/case_out.$$)"; rm -f /tmp/case_out.$$
status="${result%%$'\x1f'*}"
assert_status "pre-existing leak outside the range is not rescanned/flagged" 0 "$status" "${result#*$'\x1f'}"
rm -rf "$repo"

repo="$(new_repo)"
(
  cd "$repo"
  printf 'clean baseline\n' >base.txt
  git add base.txt
  git commit -q -m "chore: baseline"
  base="$(git rev-parse HEAD)"

  printf 'clean baseline\nfollow-up mentioning task_aabbccddeeff00112233\n' >base.txt
  git add base.txt
  git commit -q -m "fix: patch a thing"
  head="$(git rev-parse HEAD)"

  set +e
  output="$("$SCRIPT" --range "$base..$head" 2>&1)"
  status=$?
  set -e
  printf '%s\x1f%s' "$status" "$output"
) >/tmp/case_out.$$
result="$(cat /tmp/case_out.$$)"; rm -f /tmp/case_out.$$
status="${result%%$'\x1f'*}"; output="${result#*$'\x1f'}"
assert_status "a leak newly introduced within the range fails --range" 1 "$status" "$output"
assert_contains "failing --range output names the offending line" "$output" "task_aabbccddeeff00112233"
rm -rf "$repo"

repo="$(new_repo)"
(
  cd "$repo"
  printf 'a file that already mentions task_ffeeddccbbaa99887766\n' >leaky.txt
  git add leaky.txt
  git commit -q -m "seed: pre-existing history with a leak in it"
  base="$(git rev-parse HEAD)"

  # Remove the leaked line entirely; nothing new is added referencing it.
  printf 'a file that mentions nothing sensitive\n' >leaky.txt
  git add leaky.txt
  git commit -q -m "chore: clean up the note"
  head="$(git rev-parse HEAD)"

  set +e
  output="$("$SCRIPT" --range "$base..$head" 2>&1)"
  status=$?
  set -e
  printf '%s\x1f%s' "$status" "$output"
) >/tmp/case_out.$$
result="$(cat /tmp/case_out.$$)"; rm -f /tmp/case_out.$$
status="${result%%$'\x1f'*}"
assert_status "a commit that only removes a leaked line passes --range" 0 "$status" "${result#*$'\x1f'}"
rm -rf "$repo"

repo="$(new_repo)"
(
  cd "$repo"
  printf 'clean baseline\n' >base.txt
  git add base.txt
  git commit -q -m "chore: baseline"
  base="$(git rev-parse HEAD)"

  printf 'clean follow-up\n' >other.txt
  git add other.txt
  git commit -q -m "picked up in task/engineer-faketest-99990101-0000"
  head="$(git rev-parse HEAD)"

  set +e
  output="$("$SCRIPT" --range "$base..$head" 2>&1)"
  status=$?
  set -e
  printf '%s\x1f%s' "$status" "$output"
) >/tmp/case_out.$$
result="$(cat /tmp/case_out.$$)"; rm -f /tmp/case_out.$$
status="${result%%$'\x1f'*}"
assert_status "leaked ref in an in-range commit message fails --range" 1 "$status" "${result#*$'\x1f'}"
rm -rf "$repo"

repo="$(new_repo)"
(
  cd "$repo"
  printf 'clean baseline\n' >base.txt
  git add base.txt
  git commit -q -m "chore: baseline"
  base="$(git rev-parse HEAD)"

  mkdir -p .github/scripts
  printf 'fixture: task_aabbccddeeff00112233\n' >.github/scripts/check-internal-ids.sh
  git add .github/scripts/check-internal-ids.sh
  git commit -q -m "chore: only touches the guard's own excluded script"
  head="$(git rev-parse HEAD)"

  set +e
  output="$("$SCRIPT" --range "$base..$head" 2>&1)"
  status=$?
  set -e
  printf '%s\x1f%s' "$status" "$output"
) >/tmp/case_out.$$
result="$(cat /tmp/case_out.$$)"; rm -f /tmp/case_out.$$
status="${result%%$'\x1f'*}"
assert_status "a range touching only the guard's own excluded paths passes --range" 0 "$status" "${result#*$'\x1f'}"
rm -rf "$repo"

# =============================================================================
# misuse
# =============================================================================

set +e
output="$("$SCRIPT" 2>&1)"
status=$?
set -e
assert_status "no args exits 2 (misuse, not a false pass/fail)" 2 "$status" "$output"

echo ""
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
