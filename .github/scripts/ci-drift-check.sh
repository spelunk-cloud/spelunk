#!/usr/bin/env bash
# The Windows test leg runs cargo directly: there is no make on the runner image.
# Every other CI leg calls a make target, so the Makefile is the single source of
# truth. This asserts the Windows leg still runs exactly what `make test-config`
# runs, so the two cannot drift apart unnoticed.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

workflow=".github/workflows/ci.yml"

# Collapse internal whitespace and trim, so formatting alone is never a diff.
normalize() { awk '{ $1 = $1; print }'; }

# What make runs. TEST_FLAGS is empty: the Windows matrix leg keeps default
# features. `make -n` prints recipe lines without running them, so this needs no
# toolchain beyond make itself.
make_cmds="$(make -n test-config TEST_FLAGS='' | grep '^cargo ' | normalize)"

# What the workflow's Windows steps run, with the matrix placeholder stripped.
wf_cmds="$(sed -n '/ci-drift:begin windows-test/,/ci-drift:end windows-test/p' "$workflow" \
	| grep '^[[:space:]]*run:[[:space:]]*cargo ' \
	| sed -e 's/^[[:space:]]*run:[[:space:]]*//' -e 's/[$]{{ matrix\.test-flags }}//g' \
	| normalize)"

if [ -z "$make_cmds" ]; then
	echo "ERROR: found no cargo commands in 'make -n test-config'."
	echo "       The drift guard cannot verify anything. Fix the Makefile or this script."
	exit 1
fi

if [ -z "$wf_cmds" ]; then
	echo "ERROR: found no cargo commands between the ci-drift sentinels in $workflow."
	echo "       Keep the Windows test steps inside the sentinel comments."
	exit 1
fi

if [ "$make_cmds" != "$wf_cmds" ]; then
	echo "ERROR: $workflow and the Makefile have drifted."
	echo ""
	echo "The Windows test leg runs cargo directly (no make on the runner image),"
	echo "so it must be kept identical to what 'make test-config' runs."
	echo ""
	echo "--- make test-config   +++ $workflow (windows-test)"
	diff -u <(printf '%s\n' "$make_cmds") <(printf '%s\n' "$wf_cmds") || true
	exit 1
fi

echo "ok: the Windows test leg matches 'make test-config'"
