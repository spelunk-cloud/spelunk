#!/usr/bin/env bash
# The Windows test leg runs cargo directly: there is no make on the runner image.
# Every other CI leg calls a make target, so the Makefile is the single source of
# truth. This asserts the Windows leg still runs exactly what `make test-config`
# runs, so the two cannot drift apart unnoticed.
#
# The workflow side is enumerated structurally: every step of the `test` job that
# can reach Windows is found by its `if:`, not by markers. A guard that only
# inspects a marked region can be pointed away from the drift by the same edit
# that introduces it.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

workflow=".github/workflows/ci.yml"

# Collapse internal whitespace and trim, so formatting alone is never a diff.
normalize() { awk '{ $1 = $1; print }'; }

# Every cargo command the Windows leg of the `test` job runs, one per line.
#
# Exits 3 on any construct it cannot classify (an unrecognised `if:`, a step-level
# `env:`, tab indentation). Refusing to guess is what keeps a step the guard does
# not understand from passing silently.
wf_cargo_cmds() {
	awk '
	function fail(msg) {
		print "ERROR: the drift guard cannot evaluate " FILENAME ": " msg > "/dev/stderr"
		aborted = 1
		exit 3
	}

	# 1 = runs on Windows, 0 = never, -1 = cannot tell.
	function classify(e, seen,   x) {
		if (!seen) return 1            # no `if:` means every matrix leg, Windows included
		x = e
		gsub(/"/, "'"'"'", x)
		sub(/^\$\{\{[ \t]*/, "", x)
		sub(/[ \t]*\}\}$/, "", x)
		gsub(/[ \t]+/, " ", x)
		sub(/^ /, "", x)
		sub(/ $/, "", x)
		if (x == "runner.os == '"'"'Windows'"'"'") return 1
		if (x == "matrix.os == '"'"'windows-latest'"'"'") return 1
		if (x == "runner.os != '"'"'Windows'"'"'") return 0
		if (x == "matrix.os != '"'"'windows-latest'"'"'") return 0
		if (x ~ /^runner\.os == '"'"'(Linux|macOS)'"'"'$/) return 0
		return -1
	}

	function finish_step(   i, has_cargo, reach) {
		if (!step_open) return
		has_cargo = 0
		for (i = 1; i <= nrun; i++)
			if (runs[i] ~ /^cargo(\.exe)?[ \t]/) has_cargo = 1
		if (has_cargo) {
			reach = classify(cur_if, has_if)
			if (reach < 0)
				fail("step \"" cur_name "\" runs cargo under an `if:` the guard does not " \
				     "recognise, so it cannot tell whether Windows runs it: if: " cur_if)
			if (reach == 1) {
				if (cur_env)
					fail("step \"" cur_name "\" runs cargo on Windows with a step-level `env:`. " \
					     "The guard compares commands, not environments, so it cannot verify this " \
					     "step matches `make test-config`.")
				for (i = 1; i <= nrun; i++)
					if (runs[i] ~ /^cargo(\.exe)?[ \t]/) print runs[i]
			}
		}
		step_open = 0; nrun = 0; cur_if = ""; has_if = 0; cur_env = 0; cur_name = ""
	}

	{
		if ($0 ~ /^[ ]*\t/) fail("tab indentation at line " NR)
		match($0, /^ */); ind = RLENGTH
		rest = substr($0, ind + 1)
	}

	# Inside a `run: |` block every sufficiently indented line is content, including
	# one starting with `#` (a shell comment, not a YAML one).
	in_block {
		if (rest == "") next
		if (ind >= blk_indent) { runs[++nrun] = rest; next }
		in_block = 0
	}

	rest == "" { next }
	substr(rest, 1, 1) == "#" { next }

	ind == 0 {
		finish_step(); in_steps = 0; job = ""
		if (rest ~ /^[A-Za-z0-9_.-]+:/) { top = rest; sub(/:.*$/, "", top) }
		next
	}

	top != "jobs" { next }

	ind == 2 && rest ~ /^[A-Za-z0-9_.-]+:/ {
		finish_step(); in_steps = 0
		job = rest; sub(/:.*$/, "", job)
		next
	}

	job != "test" { next }

	ind == 4 && rest ~ /^[A-Za-z0-9_.-]+:/ {
		finish_step()
		k = rest; sub(/:.*$/, "", k)
		in_steps = (k == "steps")
		next
	}

	!in_steps { next }

	ind == 6 {
		if (rest !~ /^- /) fail("unexpected non-list line in the test job steps at line " NR)
		finish_step()
		step_open = 1
		rest = substr(rest, 3)
		ind = 8
	}

	ind == 8 && step_open && rest ~ /^[A-Za-z0-9_.-]+:/ {
		k = rest; sub(/:.*$/, "", k)
		v = rest; sub(/^[A-Za-z0-9_.-]+:[ \t]*/, "", v)
		if (k == "name") cur_name = v
		else if (k == "if") { cur_if = v; has_if = 1 }
		else if (k == "env") cur_env = 1
		else if (k == "run") {
			if (v ~ /^[|>][-+0-9]*$/) { in_block = 1; blk_indent = 9 }
			else if (v == "") fail("empty `run:` at line " NR)
			else runs[++nrun] = v
		}
		next
	}

	END { if (aborted) exit 3; finish_step() }
	' "$workflow"
}

# What make runs. TEST_FLAGS is empty: the Windows matrix leg keeps default
# features. `make -n` prints recipe lines without running them, so this needs no
# toolchain beyond make itself.
#
# Capture before filtering: under `set -e` a grep that matches nothing would abort
# the assignment and kill the script before it could say why.
if ! make_raw="$(make -n test-config TEST_FLAGS='' 2>&1)"; then
	echo "ERROR: 'make -n test-config' failed, so the drift guard cannot evaluate"
	echo "       what make runs. Fix the Makefile. make said:"
	echo ""
	printf '%s\n' "$make_raw"
	exit 1
fi
make_cmds="$(printf '%s\n' "$make_raw" | { grep -E '^cargo([ 	]|$)' || true; } | normalize)"

# The matrix placeholder is stripped: the guard compares command shape, and passes
# make the same empty TEST_FLAGS. Which config the Windows leg covers is a matrix
# decision, and `make test` covers every config either way.
if ! wf_raw="$(wf_cargo_cmds)"; then
	exit 1
fi
wf_cmds="$(printf '%s\n' "$wf_raw" | sed -e 's/[$]{{ matrix\.test-flags }}//g' | normalize)"

if [ -z "$make_cmds" ]; then
	echo "ERROR: 'make -n test-config' ran, but emitted no cargo commands."
	echo "       The drift guard has nothing to compare, so it cannot verify the"
	echo "       Windows test leg. Restore the cargo gates in the test-config target"
	echo "       of the Makefile (nextest + doctest), or update this script."
	exit 1
fi

if [ -z "$wf_cmds" ]; then
	echo "ERROR: found no Windows cargo steps in the test job of $workflow."
	echo "       The Windows leg has no make on the runner image, so it must run"
	echo "       cargo directly in steps guarded by: if: runner.os == 'Windows'."
	echo "       Expected it to run:"
	echo ""
	printf '%s\n' "$make_cmds" | sed 's/^/         /'
	exit 1
fi

if [ "$make_cmds" != "$wf_cmds" ]; then
	echo "ERROR: $workflow and the Makefile have drifted."
	echo ""
	echo "The Windows test leg runs cargo directly (no make on the runner image),"
	echo "so it must be kept identical to what 'make test-config' runs."
	echo ""
	echo "--- make test-config   +++ $workflow (Windows steps of the test job)"
	diff -u <(printf '%s\n' "$make_cmds") <(printf '%s\n' "$wf_cmds") || true
	exit 1
fi

echo "ok: the Windows test leg matches 'make test-config'"
