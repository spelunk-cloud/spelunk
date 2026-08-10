# Stability contract

This document says which parts of spelunk you may build on, and what a version
bump is allowed to do to them.

spelunk follows [Semantic Versioning](https://semver.org/). Before 1.0 that
promise is not yet in force: the surfaces below are already treated as stable in
practice, and this document is what they are frozen against when 1.0 ships.

Every surface is one of three things:

| Level | Promise |
|---|---|
| **Stable** | Frozen for the life of a major version. Changes are additive only. Anything removed, renamed, or retyped requires a major bump, after a deprecation period. |
| **Best-effort** | Changes are avoided and announced in the changelog, but a minor release may still change them. Depend on it if you must; pin your version if you do. |
| **Internal** | No promise at all. May change in any release without notice. Do not build on it. |

If a surface is not listed here, treat it as **internal**.

## What is not stable

Stating this first, because it is the part most often assumed:

- **Porcelain output.** The human-readable text of `search`, `status`, `context`,
  `graph`, `memory`, and every other non-plumbing command. Colours, column
  widths, wording, ordering, and summary lines all change freely. Parsing them
  with `grep`/`awk` will break. Use the plumbing commands instead, or one of the
  structured `--format` modes covered under
  [Structured output from porcelain commands](#structured-output-from-porcelain-commands).
- **Log and tracing text.** Message wording, `tracing` targets, span names, and
  log levels. Diagnostics on stderr from any command, including the diagnostics
  that accompany a plumbing exit 2, are advisory text and not a parseable
  interface. The *exit code* is the interface.
- **Internal crate APIs.** `spelunk-core`, `spelunk-cli`, `spelunk-embed`, and
  `spelunk-server` are workspace members, not published to crates.io. Any Rust
  item they expose, `pub` or not, may change in any release. Depending on them
  as a git dependency is unsupported.
- **The `/local/` HTTP routes.** `spelunk-server` registers `/local/relay/push`,
  `/local/relay/poll`, and `/local/relay/ack` outside the documented API. They
  are deliberately absent from `docs/openapi.json` and are internal transport
  between a spelunk client and its own server.
- **Wire-format details of the index.** Embedding vectors, the exact SQL schema
  of any table, and sqlite-vec internals. The *file* is a compatibility surface
  (see On-disk formats below); the SQL inside it is not.

## CLI

**Stable:** command names, subcommand names, flag names (long form), positional
argument order, and exit codes, for every command listed in `spelunk --help`.

- New commands and new flags may be added in a minor release.
- A flag's default value may change only in a major release.
- Short flags (`-d`) are **best-effort**: they are stable in practice, but a
  collision with a new long flag may force a reassignment.
- Hidden flags (clap `hide = true`, for example `publish-notes`' positional
  remote URL) are **internal**, present only for compatibility with the callers
  that pass them. The exception is a hidden flag kept as a deprecated alias of a
  stable one: `check --porcelain` is hidden but still honoured, and means
  `check --format porcelain`. Those follow the deprecation policy below rather
  than the internal rule.

### Exit codes

The plumbing exit codes are the most load-bearing part of the CLI contract,
because scripts branch on them. They are **stable**:

| Code | Meaning |
|---|---|
| `0` | Succeeded, one or more results emitted on stdout. |
| `1` | Succeeded, no results. An empty set, **not** an error. |
| `2` | Hard error. Diagnostics on stderr, **stdout is empty**. |

A script must distinguish `1` from `2`. Treating any non-zero exit as fatal is
wrong: `1` means the query was valid and matched nothing.

Three commands cannot return `1`, by construction, and this is part of the
contract rather than an oversight:

| Command | Why it never returns 1 |
|---|---|
| `hash-file` | A readable file always has a hash, so there is always exactly one result. |
| `embed` | Empty stdin is an empty *input*, not an empty result set. It exits `0` having emitted nothing. |
| `publish-notes` | It runs from a `pre-push` hook, where a non-zero exit aborts the user's branch push. Both "nothing to publish" and, under `--best-effort`, a publish failure exit `0` and report the outcome in the JSON payload. |

Porcelain commands use `0`/`1` with their own documented meanings (`check`
exits `1` when the index is stale, for example) and do not follow the plumbing
convention.

### Structured output from porcelain commands

Most porcelain commands take a `--format` flag that switches stdout from the
human-readable text above to a machine-readable shape: `json` everywhere,
plus `jsonl` on `search`, `graph`, `memory list`, and `memory since`, and
`porcelain` on `check`. This is a **different surface** from the text output,
and a different one again from plumbing JSONL: none of it is covered by the
plumbing golden schema.

In `--format porcelain`, stdout carries **only** the stable `key=value` lines
(and, with `--files`, the stale paths). Human diagnostics that `check` also
computes — the server-reachability line, the active-intent list, and the
file-overlap warning — are written to **stderr** in this mode, so a pipe over
stdout stays machine-parseable while a human at the terminal still sees them.
In text mode those diagnostics remain on stdout.

| Surface | Level |
|---|---|
| `spelunk status --format json` | **Stable** for its core fields, on the same additive-only terms as plumbing JSONL: new optional fields may appear, existing ones are not renamed or removed, and consumers must tolerate unknown fields. The field list is documented on the `status` handler in `crates/spelunk-cli/src/cli/cmd/status.rs`. |
| Every other `--format json`, `--format jsonl`, or `--format porcelain` mode | **Best-effort**. Structured, and reasonable to script against, but not enforced by a golden schema. Changes are avoided and go in the changelog; pin your version if you depend on the exact shape. |

`status --format json` also emits a set of richer fields for tooling (`tier`,
`mode`, `sync_pending`, `sync_last_synced_at`, `server_url`, `capabilities`,
`embedder_state`, `embedding_count`, `embedding_pending`, `embed_worker_alive`,
`embed_tokens`, `drift_candidates`, `usage_7d`) that are explicitly **not** in
the stable set and may change or disappear in a minor release.

If you need a surface with a test-enforced schema, use the plumbing commands.

## Plumbing JSONL

**Stable:** for every `spelunk plumbing <command>`, the *name and type* of each
field in the emitted JSON objects, and the guarantee that stdout is newline
delimited JSON with exactly one object per line.

Not stable: field **order** within an object, the **number** of lines, and the
**values** themselves (line numbers, hashes, scores, and timestamps all move).

### Evolution rule: additive only

Within a major version:

- **Allowed:** adding a new field. A consumer that ignores unknown fields is
  unaffected, and every consumer is expected to ignore unknown fields.
- **Not allowed:** removing a field, renaming a field, changing a field's JSON
  type (including widening an integer to a float), or making a
  previously-always-present field conditional.

A field documented as optional may legitimately be absent. Those are fields the
serializer skips when unset; they are listed as `optional` in the golden schema
described under [Enforcement](#enforcement).

## Server HTTP API

**Stable:** every route under `/v1/`, as described by `docs/openapi.json`. That
covers paths, methods, request and response schemas, and status codes.

Within `/v1/`:

- **Allowed:** new routes, new optional request fields, new response fields, new
  enum values in a field documented as open.
- **Not allowed:** removing a route or method, removing or renaming a response
  field, making an optional request field required, narrowing an accepted type,
  or changing the meaning of a status code.

Anything outside `/v1/` is internal. `GET /api-docs/openapi.json` serves the
spec from the running binary and is **best-effort**: useful for tooling, but not
a route to build a product on.

`info.version` inside the spec is a placeholder and does not track the crate
version. Use `GET /v1/health` for the server's real version.

## Config

**Stable:** the key names, types, and defaults documented in
[Config reference](config-reference.md).

**Also stable, and just as load-bearing: which file a key may be set in.** A key
is not simply "supported"; it is supported in a specific place. Three keys are
called out by name, because each is one a reader would otherwise reasonably
guess wrong about, and each restriction is part of the contract:

- `server_url` is **ignored in the global personal config**
  (`~/.config/spelunk/config.toml`, including a file passed to `--config`). It
  may come only from the checked-in `.spelunk/config.toml` or from
  `SPELUNK_SERVER_URL`. Everyone working on a project needs the same team
  server, which a per-developer file cannot guarantee. A global config that
  still sets it loads fine; the value is discarded.
- `server_key` is **ignored in the project config** (`.spelunk/config.toml`). A
  repository must never be able to hand a secret to whoever clones it. Use
  `spelunk auth set-key --server <url>`, `spelunk login`, or
  `SPELUNK_SERVER_KEY`.
- `llm_url` is **ignored in the project config**, which follows from the
  allowlist below rather than being an exception to it, and is named here
  because it looks like `server_url` and is not. An LLM endpoint is a
  per-developer choice: a committed value points every teammate's local daemon
  at whichever machine the author was running a model on. Set it in the
  personal config or via `SPELUNK_LLM_URL`. Its credential is not a config key
  in either file (`spelunk auth set-key --llm` or `SPELUNK_LLM_KEY`), on the
  same reasoning as `server_key`.

Beyond those three:

- Unrecognised keys are ignored rather than rejected. A config written for a
  newer spelunk still loads on an older one, and a config carrying a removed key
  still loads. A key ignored because it is in the wrong file behaves the same
  way: the rest of the file is unaffected.
- The **project-level allowlist** is itself stable. A checked-in
  `.spelunk/config.toml` is honoured for exactly `server_url`, `project_id`,
  `server_ca`, and `[index]`. Adding a key to that allowlist is additive and
  allowed; removing one is a breaking change.
- Environment variable overrides (`SPELUNK_*`) are stable on the same terms as
  the keys they override. They are not subject to the file restrictions above:
  `SPELUNK_SERVER_URL`, `SPELUNK_SERVER_KEY`, and `SPELUNK_LLM_URL` all take
  effect wherever they are set. What a variable set to an **empty** value does
  is documented in [Config reference](config-reference.md) but is not frozen
  here.

### Deprecation policy

Removing or renaming a stable config key follows a fixed sequence:

1. **Alias.** The old key keeps working, mapped onto the new one, for at least
   one full minor release.
2. **Warn.** While the alias still works, using it emits a deprecation warning
   on stderr naming the replacement. The warning lives and dies with the alias:
   once the key is gone there is no warning, because a load-time message whose
   only job is to describe a key that no longer does anything is permanent code
   for a one-release problem. See
   [ADR-071](adr/071-per-server-client-bearer-scoping.md) for the reasoning.
3. **Remove.** The key is dropped in the next major release, and listed under
   "Removed fields" in [Config reference](config-reference.md) and under
   `### Removed` in the changelog. It then falls back to the
   ignore-unknown-keys rule, so old configs still load; they just stop having
   that effect.

The same three steps apply to CLI flags and to `/v1/` request fields.

#### Worked example: `memory_server_url`

This is the precedent the policy is written from.

1. **Alias.** `server_url` carried `#[serde(alias = "memory_server_url")]`, and
   `server_key` carried `#[serde(alias = "memory_server_key")]`, so an existing
   config kept working untouched. The environment variable
   `SPELUNK_MEMORY_SERVER_URL` was accepted as a fallback for
   `SPELUNK_SERVER_URL`.
2. **Warn.** Partially, and this is where the precedent falls short of the
   policy above rather than setting it. The environment fallback did warn:
   `SPELUNK_MEMORY_SERVER_URL is deprecated; use SPELUNK_SERVER_URL instead`.
   The two TOML aliases never warned at all. They were accepted silently for
   their whole deprecation window, so the only signal a user got was the
   changelog. Step 2 is written as a requirement for what comes next, not as a
   description of what this example did.
3. **Remove.** The aliases and the environment fallback were deleted, the
   changelog recorded the break, and `docs/config-reference.md` gained a
   "Removed fields" row pointing at the replacement. The keys are now unknown
   fields: a config that still carries them loads fine and keeps every other
   field, but the deprecated keys have no effect. Regression tests in
   `crates/spelunk-core/src/config/mod.rs` pin exactly that, so the removal
   cannot silently regress into a partial mapping.

That removal shipped pre-1.0, which is why it landed in a minor release rather
than a major one. After 1.0, step 3 waits for the next major version.

## On-disk formats

The promise here is **forward compatibility of your data**: an upgrade must
never require you to delete a store and rebuild it, and must never lose a
recorded memory. The promise is *not* that the SQL schema stays fixed.

| Store | Versioning | Level |
|---|---|---|
| `.spelunk/index.db` | `PRAGMA user_version`, migrated forward on open | **Stable**: migrations are forward-only and run automatically. The index is derived data, so a rebuild is always a valid recovery. |
| `.spelunk/memory.db` | `PRAGMA user_version`, independent of the index | **Stable**, and stricter: memory is not derived data and cannot be rebuilt. A store from a newer spelunk is refused with an upgrade message rather than opened and damaged. |
| `~/.config/spelunk/registry.db` | none | **Best-effort**. Tables are created idempotently. It holds project registrations, which are re-derivable by re-registering. |
| git notes on `refs/notes/spelunk` | `schema_version` inside each JSON record | **Stable**. A record with a higher `schema_version` than the reader knows is refused rather than misread, and lines that are not spelunk records are left untouched, so the ref can be shared with other tooling. |
| server-side database | sequential migration files | **Internal** to a server deployment, and not a client-facing surface. |

Migrations are **forward-only**. Downgrading spelunk after an upgrade has
migrated a store is not supported.

### Downgrading, and why `user_version` can go backwards

"Not supported" does not mean "prevented", and the two stores behave
differently when an older binary opens a newer one. Both behaviours below were
measured against real released binaries by the
[upgrade corpus](../scripts/upgrade-corpus/README.md), not inferred.

**`memory.db` refuses.** A store stamped above the build's own
`MEMORY_SCHEMA_VERSION` is rejected with an upgrade message, which is the row
in the table above. Memory is not derived data, so refusing is the designed
outcome.

**`index.db` does not refuse.** It reads cleanly and re-stamps its
`PRAGMA user_version` down to its own. If you are debugging an `index.db` whose
`user_version` appears to have gone *backwards*, this is what happened: an
older spelunk opened it. It is not corruption, and nothing was lost.

The rewind is not a quirk of one release. It falls out of how the migration
runner works in every build: it returns early only when the stamp already
equals its own `CURRENT_SCHEMA_VERSION`, and otherwise runs whatever steps are
above the value it read and stamps its own version at the end. A stamp *above*
its own is therefore written back down. Concretely, v0.9.3 rewinds an
`index.db` that a current build had stamped 15 back to 14.

It self-heals. The steps above the rewound version are individually idempotent,
so the next open by a current build re-runs them as no-ops and re-stamps the
current version. No row is lost in either direction.

What makes this safe rather than merely survivable is the invariant that a
binary never leaves the stamp *above* its own version. If it did, a newer build
would skip migrations it had never actually run, and that is the case where
data would be damaged. The corpus asserts that bound directly.

### `.spelunk/` layout

**Stable:** the directory name `.spelunk/` at the project root, and the names
`config.toml`, `index.db`, and `memory.db` within it. Tooling may rely on
`.spelunk/index.db` marking a project root, which is how spelunk itself
discovers one.

**Internal:** everything else in that directory, including lock files, pid
sidecars, and background logs. Names, formats, and existence may change, and an
internal file may be removed outright.

`~/.config/spelunk/` (config and registry) and `~/.local/state/spelunk/`
(runtime state for the local server) follow the same split: the config file is
stable, the state files are internal.

## Enforcement

A contract nothing checks is a wish. Each promise above is tied to something
that fails CI when it is broken.

| Promise | Enforced by |
|---|---|
| Plumbing JSONL field names and types | `crates/spelunk-cli/tests/golden/plumbing_jsonl_schema.json` plus `crates/spelunk-cli/tests/plumbing_jsonl_contract.rs`. Each command is run for real and its output checked against the committed schema. Required fields must be present and correctly typed; **undeclared fields are accepted**, so additive change passes and removal, rename, or retype fails. |
| Every plumbing command has a declared schema | `golden_schema_covers_every_plumbing_subcommand`, which reads the command list out of clap's own help, so a newly added command cannot ship as an unguarded stable surface. |
| The checker itself actually rejects things | `crates/spelunk-cli/tests/schema_contract_checker.rs`. Without it, a checker that accepted everything would leave every golden file green. It drives removal, rename, and retype across every field of every declared command, and pins the reporting wrapper too, including its refusal of a command that emitted no rows at all. |
| Each declared field is load-bearing, per command | `assert_every_declared_field_is_load_bearing`, run inside every command's conformance test in `plumbing_jsonl_contract.rs`. The command's real output is replayed with one declared field dropped, then retyped, and the checker must object each time. Conformance alone would pass against a checker that never rejects anything. |
| Plumbing exit codes 0/1/2 | `crates/spelunk-cli/tests/plumbing_exit_codes.rs`, covering all three codes for every command, including the stdout-is-empty guarantee on exit 2 and the three documented exceptions. |
| `/v1/` matches `docs/openapi.json` | The `openapi-snapshot` job in `.github/workflows/ci.yml`. The spec is generated from the running binary (`cargo run -p spelunk-server -- --print-openapi`) and diffed against the committed file, so a route or schema change that skips regenerating the snapshot fails CI. |
| On-disk forward compatibility, for every store above | `crates/spelunk-cli/tests/upgrade_corpus.rs`, run by `.github/workflows/upgrade-corpus.yml`. Artifacts written by **real released binaries** are opened with the current build and checked for surviving rows, content, embeddings, the entity-id backfill, and full-text hits, plus upgrade idempotence. Every other migration test in the repo builds an old shape by hand, which tests what we believe the old format was; this one tests what it is. See [the upgrade corpus](../scripts/upgrade-corpus/README.md). |
| The stamp is never left above the opening build's version | The same suite, against a pinned older release opening a current store. This is the bound that keeps the `user_version` rewind safe, since a newer build must never skip migrations it has not run. |
| The above run on every change | `.github/workflows/stability-contract.yml`. |

### Changing a stable surface deliberately

If a change to a stable surface is intended:

1. Confirm it is additive. If it is, the golden schema needs no edit, and the
   tests already pass.
2. If it is not additive, it is a breaking change. It needs a major version, a
   deprecation period first, and a changelog entry under `### Removed` or
   `### Changed`.
3. For the server, regenerate the spec:
   `cargo run -p spelunk-server -- --print-openapi > docs/openapi.json`.
4. Update the golden schema and this document in the same change, so the
   contract and the code never disagree.

## What's next

- [Version skew](version-skew.md): what happens when the two ends of a
  connection are different versions
- [Plumbing and porcelain](plumbing-and-porcelain.md): why the split exists and
  how to script against it
- [Commands](commands.md): the full CLI reference
- [Config reference](config-reference.md): every key, default, and env override
- [Releasing](releasing.md): how a version is cut
