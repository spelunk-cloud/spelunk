#!/usr/bin/env bash
#
# Version-skew smoke test: drive one real `spelunk` binary against one real
# `spelunk-server` binary through the end-to-end memory flow.
#
#   usage: skew-smoke.sh <path-to-spelunk> <path-to-spelunk-server>
#
#   SKEW_EMBEDDER_TIMEOUT_SECS  wall-clock wait for the embedder (default 300)
#   SKEW_MODEL_CACHE            model directory kept outside the isolated HOME,
#                               so the model is downloaded once rather than per run
#   SKEW_ALLOW_SKIPPED_SEARCH   set to 1 to pass without exercising search
#
# Every other contract test in this repo talks to a mock or a fixture written
# to the shape we *believe* a peer has. This script is the only one that puts
# two independently built artifacts on a socket together, so it is the only one
# that can falsify that belief. CI runs it in both directions: current CLI
# against the previous released server, and the previous released CLI against
# the current server.
#
# See docs/version-skew.md for the support window this is asserting.

set -euo pipefail

CLI_BIN="${1:?usage: skew-smoke.sh <spelunk> <spelunk-server>}"
SERVER_BIN="${2:?usage: skew-smoke.sh <spelunk> <spelunk-server>}"

for bin in "$CLI_BIN" "$SERVER_BIN"; do
  [ -x "$bin" ] || { echo "FAIL: not an executable: $bin" >&2; exit 1; }
done

# Resolved to absolute paths up front, because the steps below invoke the CLI
# from inside a scratch project directory. A relative path like
# `target/release/spelunk` silently stops resolving after that `cd`, which is
# how CI would be invoking this.
CLI_BIN="$(cd "$(dirname "$CLI_BIN")" && pwd)/$(basename "$CLI_BIN")"
SERVER_BIN="$(cd "$(dirname "$SERVER_BIN")" && pwd)/$(basename "$SERVER_BIN")"

# Reduce the environment to an allowlist before anything is launched. Every
# path these binaries resolve for config, state, registry, model and secrets is
# selected by an environment variable, and each of those is consulted *before*
# HOME, so overriding HOME cannot cover any of them by construction.
#
# An allowlist rather than a list of known overrides, because the list has been
# wrong every time it has been checked: three successive audits each found more
# than the last, and the namespace moves on its own (SPELUNK_OLD_BINARY arrived
# and SPELUNK_NO_SLUG_CACHE left while this branch was open). Scrubbing by the
# SPELUNK_ prefix is not enough either: CREDENTIALS_DIRECTORY is read by
# spelunk-server to resolve its API key from systemd LoadCredential=, and is a
# secret path with no prefix at all. Anything not named below is gone, so a
# variable added to the tree later is excluded by default rather than by
# memory.
#
# Why this is not cosmetic: SPELUNK_SECRET_STORE=file below exists to keep old
# released binaries off the OS keyring, but the file store is
# <config_dir>/secrets.toml and spelunk_config_dir() honours SPELUNK_CONFIG_DIR
# first. With that variable inherited, the guard would have pointed those
# binaries at the developer's real secrets.toml, to read and to write.

# Needed to run at all: find binaries, make temp dirs, decode UTF-8. None of
# them selects spelunk state.
#
# SSL_CERT_FILE and SSL_CERT_DIR were permitted here, justified as needed to
# verify TLS for the model download. That premise was false: nothing in this
# workspace reads either one. reqwest is pinned to rustls-tls against
# webpki-roots, and neither rustls-native-certs nor openssl-probe appears in
# Cargo.lock. An entry nothing reads is not inert, it is a standing permission,
# and this pair would become a blessed trust-redirect the day someone switches
# to native roots. Removed rather than re-justified.
#
# `_`, PWD, OLDPWD and SHLVL are bash's own bookkeeping, re-set by the shell
# whatever we do here. SHELLOPTS and BASHOPTS are named for a narrower reason:
# bash marks them readonly, so the scrub below cannot unset them, and the
# backstop would then abort the run reporting that the binaries had been pointed
# at real user state, which is untrue of a shell option list.
INHERITED_ENV_ALLOWLIST="PATH TMPDIR TMP TEMP LANG LC_ALL LC_CTYPE TERM USER LOGNAME SHELL _ PWD OLDPWD SHLVL SHELLOPTS BASHOPTS"

# Set by this script itself, below. Listed so the assertion can tell what it
# owns from what leaked in. HOME is owned, not inherited: the script overwrites
# it unconditionally, and filing it as inherited declared the invoking user's
# real HOME permitted, which is the first version of the hole this allowlist
# exists to close.
OWNED_ENV="HOME SPELUNK_SECRET_STORE SPELUNK_SERVER_URL SPELUNK_STATE_DIR XDG_CONFIG_HOME XDG_STATE_HOME XDG_DATA_HOME"

# SKEW_* are this script's own knobs, read here and by nothing under test.
env_is_allowed() {
  case "$1" in
    SKEW_*) return 0 ;;
  esac
  case " $INHERITED_ENV_ALLOWLIST $OWNED_ENV " in
    *" $1 "*) return 0 ;;
  esac
  return 1
}

env_names() { env | sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p'; }

for _name in $(env_names); do
  env_is_allowed "$_name" || unset "$_name" 2>/dev/null || true
done
unset _name

# Backstop for a later edit that exports something new before the binaries
# start. Without it the allowlist above is only correct until someone adds a
# line under it.
assert_env_is_allowlisted() {
  local name
  for name in $(env_names); do
    env_is_allowed "$name" \
      || fail "$name reached the binaries under test; it is not one this script sets, so the run would have pointed them at real user state"
  done
}

# On macOS a child may still report __CF_USER_TEXT_ENCODING after the scrub
# above removed it. That is not a permission and could not be made one:
# CoreFoundation re-injects the variable inside CF-linked processes, below the
# level a shell can reach. A python3 launched from here sees it in os.environ; an
# `env` launched from here does not, so the assertion above, which reads this
# script's own environment, can neither observe it nor act on it. It is a
# text-encoding hint and selects no spelunk path.

# The released binaries pre-date the keychain fix and will block on a real
# macOS Keychain prompt without this. It is exported rather than passed per
# command so every child the CLI spawns inherits it too.
export SPELUNK_SECRET_STORE=file

WORK="$(mktemp -d)"
SERVER_PID=""

cleanup() {
  # Only ever kills the server this script started. A developer box may well
  # have its own spelunk-server on the default port; that one is not ours.
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# Bind an ephemeral port and immediately release it. Racy in principle; in
# practice the server binds within milliseconds and a collision fails loudly on
# the health check below rather than silently passing.
free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

# Isolation, applied before anything is launched. This used to be set after the
# server was already running, so the comment claiming it was in force was true
# of the CLI only: the server inherited the invoking user's real HOME, and with
# it their config, registry and secret store. That matters here more than
# anywhere else in the repo, because the binaries under test are old releases
# that pre-date the file secret-store default and will reach a real OS keyring.
export HOME="$WORK/home"
export XDG_CONFIG_HOME="$WORK/home/.config"
export XDG_STATE_HOME="$WORK/home/.local/state"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"

# Set explicitly because the CLI does not read XDG_STATE_HOME: it resolves this
# directory from SPELUNK_STATE_DIR or from `dirs::home_dir()`, which on Windows
# is a Registry lookup rather than $HOME. Naming it here keeps the port file
# written below and the file the CLI reads the same path on every platform.
export SPELUNK_STATE_DIR="$XDG_STATE_HOME/spelunk"
mkdir -p "$SPELUNK_STATE_DIR"

# Set explicitly rather than left to the isolated HOME: an invoking user with
# XDG_DATA_HOME already pointing at their real data dir would otherwise leak
# straight past the isolation above.
export XDG_DATA_HOME="$WORK/home/.local/share"
mkdir -p "$XDG_DATA_HOME"

# The embedder model is a large read-only download rather than user state, so
# it is the one thing a caller may keep outside the isolated HOME: without it
# the server re-downloads the model every run and never reaches `ready` inside
# the timeout. Nothing the isolation exists to protect lives here.
#
# Still a symlink, but not for the reason previously given here. SPELUNK_MODEL_DIR
# does exist (`spelunk-server --model-dir`), so "offers no override of its own"
# was simply false. It is the wrong override for this job: it selects a
# *pre-provisioned* GGUF plus tokenizer for air-gapped installs and bypasses the
# Hugging Face download path entirely, whereas what needs redirecting is that
# download path's own cache, `data_local_dir()/spelunk/models`. Setting it here
# would point the server at artifacts this script never fetched.
if [ -n "${SKEW_MODEL_CACHE:-}" ]; then
  mkdir -p "$SKEW_MODEL_CACHE"
  case "$(uname -s)" in
    Darwin) MODEL_PARENT="$HOME/Library/Application Support/spelunk" ;;
    *)      MODEL_PARENT="$XDG_DATA_HOME/spelunk" ;;
  esac
  mkdir -p "$MODEL_PARENT"
  ln -s "$SKEW_MODEL_CACHE" "$MODEL_PARENT/models"
fi

PORT="$(free_port)"
BASE="http://127.0.0.1:${PORT}"

# Explicit server URL, so the CLI talks to the binary under test and never
# auto-discovers some other server already listening on the default port. Set
# here rather than after the launch below so one assertion can cover the whole
# namespace at the moment the first binary under test starts.
export SPELUNK_SERVER_URL="$BASE"

assert_env_is_allowlisted

echo "== starting server: $SERVER_BIN on $BASE"
"$SERVER_BIN" --port "$PORT" --db "$WORK/server.db" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

HEALTH=""
for _ in $(seq 1 40); do
  HEALTH="$(curl -sf -m 2 "$BASE/v1/health" || true)"
  [ -n "$HEALTH" ] && break
  sleep 1
done
[ -n "$HEALTH" ] || { cat "$WORK/server.log" >&2; fail "server never answered /v1/health"; }

# The state-dir port file is deliberately NOT written yet. It is what makes
# this server discoverable both for loopback inference routing (needed by the
# search step below) and, as an unavoidable side effect, as an ADR-037 "local
# relay": under the default local_first mode, `memory add`'s post-write nudge
# and `memory list`/`search`'s poll_and_apply both probe that same file, and
# either would silently push+ack these entries to the server ahead of the
# explicit push/repush/sync assertions below, making them report "already
# synced" for a reason that has nothing to do with version skew. Push/sync
# only need `SPELUNK_SERVER_URL` (already exported above), so the port file is
# written further down, right before the one step that actually needs it.

SERVER_VERSION="$(printf '%s' "$HEALTH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])')"
CLI_VERSION="$("$CLI_BIN" --version | awk '{print $NF}')"
echo "== CLI $CLI_VERSION  <->  server $SERVER_VERSION"

# Without this the whole job can go green while proving nothing: point both
# arguments at the same build and every assertion below still passes. A skew
# test that is not skewed is not a test.
[ "$CLI_VERSION" != "$SERVER_VERSION" ] \
  || fail "CLI and server are both $CLI_VERSION; this run tested no skew at all"

# A fixed slug shared by both checkouts below, so the second one pulls exactly
# what the first one pushed. Left to `spelunk init` it would be a per-directory
# content hash and the pull would legitimately find nothing.
PROJECT_ID="local/skewsmoke"

make_project() {
  local dir="$1"
  mkdir -p "$dir"
  git -C "$dir" init -q .
  git -C "$dir" config user.email skew@example.invalid
  git -C "$dir" config user.name "skew smoke"
  echo 'fn main() {}' >"$dir/main.rs"
  git -C "$dir" add -A
  git -C "$dir" -c commit.gpgsign=false commit -qm "initial"
  mkdir -p "$dir/.spelunk"
  printf 'project_id = "%s"\n' "$PROJECT_ID" >"$dir/.spelunk/config.toml"
}

run() {
  local label="$1"; shift
  echo "-- $label"
  # Exit status is read straight off the command. Piping it into tee or tail
  # would report the pipeline's status instead, which is always the last stage.
  if ! ( cd "$WORK/a" && "$@" ) >"$WORK/$label.out" 2>&1; then
    cat "$WORK/$label.out" >&2
    fail "$label exited non-zero (CLI $CLI_VERSION -> server $SERVER_VERSION)"
  fi
}

make_project "$WORK/a"

run add-decision "$CLI_BIN" memory add -k decision \
  -t "Skew smoke decision" -b "Written by the CLI at version $CLI_VERSION."
run add-note "$CLI_BIN" memory add -k note \
  -t "Skew smoke note" -b "A second entry, so list and pull counts are not trivially one."

run list "$CLI_BIN" memory list
grep -q "Skew smoke decision" "$WORK/list.out" || fail "memory list lost the decision entry"
grep -q "Skew smoke note" "$WORK/list.out" || fail "memory list lost the note entry"

run push "$CLI_BIN" memory push
grep -q "created 2" "$WORK/push.out" \
  || { cat "$WORK/push.out" >&2; fail "push did not report 2 created entries across the skew boundary"; }

# Re-push must be idempotent on external_id. A server that lost that dedupe
# would look identical to a working one on the first push alone.
run repush "$CLI_BIN" memory push
grep -q "already synced" "$WORK/repush.out" \
  || { cat "$WORK/repush.out" >&2; fail "re-push was not idempotent"; }

run sync "$CLI_BIN" memory sync

# Route inference at the peer as well, now that the explicit push/repush/sync
# assertions above are done. SPELUNK_SERVER_URL does not do this: in the
# default local_first mode an explicit server_url is a memory sync replica
# only, and inference resolves through loopback auto-discovery, which reads
# this file (`capability/probe.rs` step 3a). Without it the search step below
# fails outright in CI, and on a developer box fails worse: auto-discovery
# falls through to the default port 7777, embeds against whatever
# current-version server is listening there, and reports success having
# crossed no skew boundary at all.
printf '%s\n' "$PORT" >"$SPELUNK_STATE_DIR/server.port"

# Search is the one step whose outcome depends on something other than the
# wire contract: the server embeds the query, so it needs the model loaded.
# Wait for the embedder to settle before judging the result, otherwise this
# step measures model download speed rather than version skew. An earlier
# draft of this script did exactly that and produced a convincing false
# positive: an old CLI "failing" against a new server purely because the new
# server was a debug build and was still warming up.
embedder_state() {
  curl -sf -m 2 "$BASE/v1/health" 2>/dev/null \
    | python3 -c 'import json, sys
body = json.load(sys.stdin)
embedder = body.get("embedder")
print("absent" if embedder is None else (embedder.get("state") or "unknown"))' 2>/dev/null \
    || echo unreachable
}

EMBEDDER_TIMEOUT="${SKEW_EMBEDDER_TIMEOUT_SECS:-300}"
echo "-- waiting up to ${EMBEDDER_TIMEOUT}s for embedder to settle"
# A wall-clock deadline rather than an iteration count. Counting iterations made
# the real bound up to three times this value, because each pass also pays a 1s
# sleep and a curl worth up to `-m 2`.
EMBEDDER_DEADLINE=$(( $(date +%s) + EMBEDDER_TIMEOUT ))
EMBEDDER_STATE="unknown"
while :; do
  EMBEDDER_STATE="$(embedder_state)"
  # `absent` is terminal and was previously invisible: a peer older than v0.9.x
  # publishes no `embedder` object at all, read the same as `unknown`, so every
  # run against one burned the entire timeout waiting for a state it can never
  # report. `unknown` from a peer that does publish the object stays
  # non-terminal, because that one can still resolve.
  case "$EMBEDDER_STATE" in
    ready|unavailable|disabled|absent) break ;;
  esac
  [ "$(date +%s)" -lt "$EMBEDDER_DEADLINE" ] || break
  sleep 1
done
echo "   embedder state: $EMBEDDER_STATE"

echo "-- search"
if ( cd "$WORK/a" && "$CLI_BIN" memory search "skew smoke decision" ) >"$WORK/search.out" 2>&1; then
  grep -q "Skew smoke decision" "$WORK/search.out" \
    || { cat "$WORK/search.out" >&2; fail "memory search succeeded but did not surface the decision entry"; }
elif [ "$EMBEDDER_STATE" = "ready" ]; then
  cat "$WORK/search.out" >&2
  fail "memory search failed against a ready embedder (CLI $CLI_VERSION -> server $SERVER_VERSION)"
else
  # Not a pass for search, but not a skew failure either. The one thing that
  # must still hold is that the refusal is the *documented* one: an old client
  # and a new server still agreeing on the shape of a not-ready error is
  # itself part of the contract under test. A protocol-level failure here
  # (404, 405, a deserialization error) is a real break and must not be
  # waved through by this branch.
  grep -Eqi 'embedder|embedding model|warming up|503' "$WORK/search.out" \
    || { cat "$WORK/search.out" >&2; fail "memory search failed for a reason unrelated to embedder readiness"; }

  # Skipping search is not free, and it used to be silent. It is the only step
  # that drives the query-embedding path across the skew boundary, and a warm
  # model cache on a developer box is the only reason it ever looked cheap: the
  # isolation above hands the server a cold cache, so a runner without
  # SKEW_MODEL_CACHE lands here every time. Failing by default is what stops
  # the job going green while its most valuable assertion never ran.
  if [ "${SKEW_ALLOW_SKIPPED_SEARCH:-0}" != "1" ]; then
    cat "$WORK/search.out" >&2
    fail "memory search was never exercised: embedder state=$EMBEDDER_STATE after \
waiting up to ${EMBEDDER_TIMEOUT}s, so this run asserted the wire contract but not the \
query-embedding path. Point SKEW_MODEL_CACHE at a warm model cache, raise \
SKEW_EMBEDDER_TIMEOUT_SECS, or set SKEW_ALLOW_SKIPPED_SEARCH=1 to accept the gap"
  fi
  echo "   WARNING: search skipped, embedder never became ready (state=$EMBEDDER_STATE); refusal was the documented one"
fi

# The real assertion. A second, empty checkout of the same project must be able
# to read back what the first one wrote, which exercises the response half of
# the wire contract rather than just the request half.
make_project "$WORK/b"
echo "-- pull-into-fresh-checkout"
if ! ( cd "$WORK/b" && "$CLI_BIN" memory pull ) >"$WORK/pull.out" 2>&1; then
  cat "$WORK/pull.out" >&2
  fail "pull into a fresh checkout exited non-zero"
fi

if ! ( cd "$WORK/b" && "$CLI_BIN" memory list ) >"$WORK/list-b.out" 2>&1; then
  cat "$WORK/list-b.out" >&2
  fail "memory list in the fresh checkout exited non-zero"
fi
grep -q "Skew smoke decision" "$WORK/list-b.out" \
  || { cat "$WORK/list-b.out" >&2; fail "decision entry did not survive the push/pull round trip"; }
grep -q "Skew smoke note" "$WORK/list-b.out" \
  || { cat "$WORK/list-b.out" >&2; fail "note entry did not survive the push/pull round trip"; }

echo "PASS: CLI $CLI_VERSION <-> server $SERVER_VERSION completed the memory flow"
