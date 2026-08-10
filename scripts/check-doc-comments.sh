#!/usr/bin/env bash
# Fails if test code carries rustdoc syntax (`///` or `//!`).
#
# Rustdoc is never generated for `#[cfg(test)]` modules, `#[test]` functions,
# or `crates/*/tests/` integration targets, so a doc-comment there is
# documentation syntax applied to something that will never be documentation.
# A plain `//` says the same thing without the false promise. See STYLE.md
# §2 "Documentation comments".
#
# Scope:
#   - crates/*/tests/**/*.rs: whole file, since each is entirely test code.
#     `tests/fixtures/` is excluded: it is sample input, not test code.
#   - crates/*/src/**/*.rs: two regions.
#       a) a top-level `#[cfg(test)] mod ... { .. }` body, delimited by the
#          closing `}` in column 0. rustfmt guarantees that column for a
#          top-level item, which makes this robust against braces inside
#          string literals in a way brace-counting would not be.
#       b) a doc-comment block attached directly to a `#[test]` or
#          `#[cfg(test)]` item anywhere in the file (e.g. a test-only static).
#
# Blind spot: a `#[cfg(test)] mod` nested inside another item is not detected,
# because its closing brace is indented. There are none today.
#
# Usage:
#   scripts/check-doc-comments.sh [root-dir]     # default: repo root; exit 1 on any hit
#   scripts/check-doc-comments.sh --report       # list hits, always exit 0
#   scripts/check-doc-comments.sh --self-test    # regression-tests this script
set -euo pipefail

REPORT_ONLY=0

scan_file() {
    # $1 = path, $2 = "whole" | "regions"
    awk -v path="$1" -v mode="$2" '
        function flush_pending(   i) {
            for (i = 1; i <= pending_n; i++) print path ":" pending_ln[i] "\t" pending_txt[i]
            pending_n = 0
        }
        function drop_pending() { pending_n = 0 }

        mode == "whole" {
            if ($0 ~ /^[ \t]*(\/\/\/|\/\/!)/) print path ":" NR "\t" $0
            next
        }

        # --- regions mode ---

        # Track the top-level test module body.
        in_test_mod && /^\}/ { in_test_mod = 0; next }
        in_test_mod {
            if ($0 ~ /^[ \t]*(\/\/\/|\/\/!)/) print path ":" NR "\t" $0
            next
        }

        /^#\[cfg\(test\)\]/ {
            saw_cfg_test = 1
            flush_pending()          # docs attached to a #[cfg(test)] item
            next
        }
        saw_cfg_test && /^(pub )?mod / { in_test_mod = 1; saw_cfg_test = 0; next }

        /^[ \t]*#\[test\]/ { flush_pending(); saw_cfg_test = 0; next }

        # Buffer doc-comments so we can decide once we see what they attach to.
        /^[ \t]*(\/\/\/|\/\/!)/ {
            pending_n++
            pending_ln[pending_n] = NR
            pending_txt[pending_n] = $0
            next
        }

        # Other attributes sit between the docs and the item; keep buffering.
        /^[ \t]*#\[/ { next }

        { drop_pending(); saw_cfg_test = 0 }
    ' "$1"
}

run_check() {
    local root="$1" hits total
    hits="$(
        {
            find "$root/crates" -path '*/tests/*' -name '*.rs' \
                -not -path '*/tests/fixtures/*' -print0 2>/dev/null \
                | while IFS= read -r -d '' f; do scan_file "$f" whole; done
            find "$root/crates" -path '*/src/*' -name '*.rs' -print0 2>/dev/null \
                | while IFS= read -r -d '' f; do scan_file "$f" regions; done
        } || true
    )"

    if [ -z "$hits" ]; then
        echo "check-doc-comments: ok (no rustdoc syntax in test code)"
        return 0
    fi

    total="$(printf '%s\n' "$hits" | wc -l | tr -d ' ')"
    printf '%s\n' "$hits" >&2
    echo >&2
    echo "check-doc-comments: $total doc-comment line(s) in test code." >&2
    echo "Rustdoc is not generated for test targets; use // instead. See STYLE.md §2." >&2

    [ "$REPORT_ONLY" -eq 1 ] && return 0
    return 1
}

self_test() {
    local tmp status
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    mkdir -p "$tmp/crates/demo/src" "$tmp/crates/demo/tests" "$tmp/crates/demo/tests/fixtures"

    cat >"$tmp/crates/demo/src/lib.rs" <<'EOF'
//! Module docs on production code are fine.

/// Public API docs are fine.
pub fn real() {}

/// Docs on a test-only static are not.
#[cfg(test)]
static OVERRIDE: u8 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    /// This is a violation.
    #[test]
    fn a_thing_holds() {
        // A plain comment here is fine.
        assert!(true);
    }
}
EOF
    cat >"$tmp/crates/demo/tests/it.rs" <<'EOF'
//! Violation: integration targets are all test code.
/// Violation.
#[test]
fn b() {}
EOF
    cat >"$tmp/crates/demo/tests/fixtures/sample.rs" <<'EOF'
/// Fixture input, must be ignored.
pub fn sample() {}
EOF

    status=0
    run_check "$tmp" >/dev/null 2>"$tmp/err" || status=$?

    local n
    n="$(grep -c 'lib.rs\|it.rs' "$tmp/err" || true)"
    if [ "$status" -ne 1 ]; then
        echo "self-test FAILED: expected exit 1, got $status" >&2; return 1
    fi
    if [ "$n" -ne 4 ]; then
        echo "self-test FAILED: expected 4 hits, got $n" >&2
        cat "$tmp/err" >&2; return 1
    fi
    if grep -q 'fixtures' "$tmp/err"; then
        echo "self-test FAILED: fixtures/ should be excluded" >&2; return 1
    fi
    echo "check-doc-comments: self-test ok"
}

case "${1:-}" in
    --self-test) self_test ;;
    --report)    REPORT_ONLY=1; run_check "${2:-$(git rev-parse --show-toplevel)}" ;;
    *)           run_check "${1:-$(git rev-parse --show-toplevel)}" ;;
esac
