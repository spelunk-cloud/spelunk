#!/usr/bin/env bash
# Fails if a test spawns `git` without wiring in this repo's isolation
# fixture (`isolate_git_config`/`git_command` in `spelunk_core::test_support`).
# Structural backstop for a gap that's otherwise silent: a missing call still
# passes on a clean machine, and only misbehaves on one with an ambient
# `core.hooksPath`/`commit.gpgsign`/etc.
#
# Scope:
#   - crates/*/tests/*.rs (excluding tests/common, tests/fixtures): whole
#     file, since each is entirely test code.
#   - crates/*/src/**/*.rs: only the trailing `#[cfg(test)] mod tests { .. }`
#     block, checked from the first `#[cfg(test)]` line to EOF. Assumes the
#     test module is last in the file (true today): keep it that way, or
#     this can misjudge production code as tests or vice versa.
#
# Blind spot: a spawn reached through a variable (`let bin = "git";
# Command::new(bin)`) has no literal `"git"` next to `new(` and is invisible
# to this grep-based check. Needs real dataflow analysis; out of scope here.
#
# Usage:
#   scripts/check-git-isolation.sh [root-dir]   # default: repo root
#   scripts/check-git-isolation.sh --self-test  # regression-tests this script
set -euo pipefail

ISOLATION_MARKER='fn isolate_git_config|isolate_git_config\(|git_command\(|mod common;|mod plumbing_helpers;'

# Matches `<Path>::new("git")` for any type-path prefix (covers a renamed
# `Command` import), tolerant of whitespace/line-wraps from rustfmt.
GIT_SPAWN_PATTERN='[A-Za-z_][A-Za-z0-9_]*::new *\( *"git" *,? *\)'

# Sets $fail=1 and prints an error if `$region` spawns git without an
# isolation marker. Whitespace is collapsed before matching so a
# rustfmt-wrapped call still matches as one line; markers are checked against
# the original (uncollapsed) text.
check_region() {
  local label="$1" region="$2"
  local collapsed
  collapsed="$(tr -s '[:space:]' ' ' <<<"$region")"

  if ! grep -qE "$GIT_SPAWN_PATTERN" <<<"$collapsed"; then
    return 0
  fi
  if grep -qE "$ISOLATION_MARKER" <<<"$region"; then
    return 0
  fi

  echo "ERROR: $label spawns \`git\` via a \`*::new(\"git\")\` call without wiring in git-config isolation" >&2
  echo "  (expected a call to isolate_git_config()/git_command(), or a \`mod common;\`/\`mod plumbing_helpers;\` import of the shared fixture)" >&2
  fail=1
}

run_check() {
  local root="$1"
  fail=0

  # Integration test binaries: the whole file is test code.
  while IFS= read -r -d '' f; do
    check_region "$f" "$(cat "$f")"
  done < <(find "$root/crates" -path '*/tests/*.rs' \
    -not -path '*/tests/common/*' -not -path '*/tests/fixtures/*' \
    -print0 2>/dev/null)

  # In-crate unit tests: only the trailing `#[cfg(test)] mod tests { ... }`
  # block is test code (see header comment for the "to EOF" assumption).
  while IFS= read -r -d '' f; do
    if grep -q '^#\[cfg(test)\]' "$f"; then
      check_region "$f (#[cfg(test)] region)" "$(sed -n '/^#\[cfg(test)\]/,$p' "$f")"
    fi
  done < <(find "$root/crates" -path '*/src/*.rs' -print0 2>/dev/null)

  return "$fail"
}

self_test() {
  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp:-}"' EXIT

  mkdir -p "$tmp/crates/fake-crate/tests" "$tmp/crates/fake-crate/src"

  # No isolation marker anywhere in the file: must be flagged.
  cat >"$tmp/crates/fake-crate/tests/bad.rs" <<'EOF'
#[test]
fn spawns_git_unisolated() {
    std::process::Command::new("git").arg("status").status().unwrap();
}
EOF

  # rustfmt-style line-wrapped call: a naive single-line literal-string grep
  # misses this.
  cat >"$tmp/crates/fake-crate/tests/bad_multiline.rs" <<'EOF'
#[test]
fn spawns_git_unisolated_multiline() {
    std::process::Command::new(
        "git",
    )
    .arg("status")
    .status()
    .unwrap();
}
EOF

  # Incidental extra whitespace inside the parens.
  cat >"$tmp/crates/fake-crate/tests/bad_whitespace.rs" <<'EOF'
#[test]
fn spawns_git_unisolated_whitespace() {
    std::process::Command::new( "git" ).arg("status").status().unwrap();
}
EOF

  # Renamed import: the literal substring `Command::new(` never appears.
  cat >"$tmp/crates/fake-crate/tests/bad_alias.rs" <<'EOF'
use std::process::Command as Proc;
#[test]
fn spawns_git_unisolated_alias() {
    Proc::new("git").arg("status").status().unwrap();
}
EOF

  # Wires in the shared fixture module first: must not be flagged.
  cat >"$tmp/crates/fake-crate/tests/good.rs" <<'EOF'
mod common;

#[test]
fn spawns_git_isolated() {
    common::isolate_git_config();
    std::process::Command::new("git").arg("status").status().unwrap();
}
EOF

  # No git spawn at all: must never be flagged.
  cat >"$tmp/crates/fake-crate/tests/unrelated.rs" <<'EOF'
#[test]
fn does_nothing_with_git() {
    assert_eq!(2 + 2, 4);
}
EOF

  # Production code spawns git freely above the test module; only the
  # un-isolated spawn *inside* `#[cfg(test)]` must be flagged.
  cat >"$tmp/crates/fake-crate/src/lib.rs" <<'EOF'
pub fn production_git_spawn() {
    std::process::Command::new("git").arg("rev-parse").status().unwrap();
}

#[cfg(test)]
mod tests {
    #[test]
    fn spawns_git_unisolated_in_test_module() {
        std::process::Command::new("git").arg("status").status().unwrap();
    }
}
EOF

  local failures=0

  if run_check "$tmp" 2>/tmp/self_test_out; then
    echo "SELF-TEST FAIL: run_check should have failed on the bad fixtures, but exited 0" >&2
    failures=1
  else
    if ! grep -q 'tests/bad.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: bad.rs was not flagged" >&2
      failures=1
    fi
    if ! grep -q 'tests/bad_multiline.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: bad_multiline.rs (line-wrapped call) was not flagged" >&2
      failures=1
    fi
    if ! grep -q 'tests/bad_whitespace.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: bad_whitespace.rs (extra spacing) was not flagged" >&2
      failures=1
    fi
    if ! grep -q 'tests/bad_alias.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: bad_alias.rs (renamed Command import) was not flagged" >&2
      failures=1
    fi
    if ! grep -q 'src/lib.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: the un-isolated #[cfg(test)] spawn in lib.rs was not flagged" >&2
      failures=1
    fi
    if grep -q 'tests/good.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: good.rs (isolated) was incorrectly flagged" >&2
      failures=1
    fi
    if grep -q 'tests/unrelated.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: unrelated.rs (no git spawn) was incorrectly flagged" >&2
      failures=1
    fi
  fi

  # Now remove the bad fixtures and confirm a clean tree passes.
  rm "$tmp/crates/fake-crate/tests/bad.rs" \
    "$tmp/crates/fake-crate/tests/bad_multiline.rs" \
    "$tmp/crates/fake-crate/tests/bad_whitespace.rs" \
    "$tmp/crates/fake-crate/tests/bad_alias.rs"
  sed -i.bak '/^#\[cfg(test)\]/,$d' "$tmp/crates/fake-crate/src/lib.rs" && rm -f "$tmp/crates/fake-crate/src/lib.rs.bak"
  if ! run_check "$tmp" 2>/tmp/self_test_out2; then
    echo "SELF-TEST FAIL: run_check should pass once the un-isolated spawns are removed" >&2
    cat /tmp/self_test_out2 >&2
    failures=1
  fi

  if [ "$failures" -eq 0 ]; then
    echo "check-git-isolation self-test: OK"
    return 0
  else
    return 1
  fi
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

root="${1:-.}"
if run_check "$root"; then
  echo "check-git-isolation: OK"
  exit 0
else
  echo "" >&2
  echo "See crates/spelunk-core/src/test_support.rs::isolate_git_config (the one canonical definition)." >&2
  exit 1
fi
