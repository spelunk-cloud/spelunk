# Changelog

All notable changes to spelunk are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
spelunk uses [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

### Added

- **`spelunk-export`, a standalone tool that writes a portable dump of a local
  store.** The dump is line-delimited JSON, one record per line, expressed as
  entities and relationships rather than as a copy of any table, and readable
  without special tooling. Stores are opened read-only and are never modified.
  Only authored data is carried: full-text indexes, embeddings and import
  cursors all regenerate, so carrying them would only risk carrying stale state
  forward. Every table is read inside a single read transaction, so a dump is a
  consistent point-in-time view of the whole store.

  An `inventory` subcommand reports what a store holds without touching it.

  Releases now carry `spelunk-export` as its own download for each of the four
  supported targets (`spelunk-export-<tag>-<target>.tar.gz`, `.zip` on
  Windows), separate from the `spelunk` archive and absent from the `.deb`. It
  is a single self-contained file that links no system SQLite, so it can be
  fetched on its own, verified against the sha256 digest GitHub records for the
  asset, run against a store and deleted again, without installing anything.

## [0.9.7] — 2026-08-05

### Added

- **`spelunk sync` now propagates `relates_to` links to a hosted project.**
  `memory add --relates-to` records the link locally, but sync never sent it
  upstream, so a shared knowledge graph showed related entries as unconnected
  nodes. Sync now pushes those links as part of the normal push, once both ends
  of a link have themselves synced, and posts nothing when there is nothing new.
  Link push is best-effort: a failure warns and is retried on a later sync
  rather than failing the entry push. `supersedes` links continue to travel with
  their entry, and `contradicts` links are still generated server-side.

### Changed

- **The team `spelunk-server`'s memory read endpoints now return an object
  envelope instead of a bare JSON array.** `GET /memory` and `POST
  /memory/search` return `{ "entries": [...], "total": N }`, and `GET
  /memory/harvested-shas` returns `{ "shas": [...] }`. A JSON response root is
  now always an object, never a bare array (ADR-076: the memory wire contract).

  The CLI reads both shapes, so a newer CLI keeps working against an older team
  server across the version-skew support window. A CLI released before this
  change cannot read the envelope from a newer server on these three endpoints;
  upgrade the CLI to match. See `docs/version-skew.md`. The `GET
  /memory/since?t=` legacy mode is unchanged, and still returns a bare array.

### Fixed

- **Symbols with non-ASCII characters are now reachable from `spelunk graph`.**
  A reference to a symbol such as `café` was split at the first non-ASCII
  character into a fragment that matched nothing, so non-ASCII identifiers
  produced no graph edges and were invisible to reverse lookups, even though
  their definitions were indexed correctly. Identifiers now accept Unicode
  alphanumerics, and the length bound counts characters rather than bytes so
  multibyte names are bounded the same way as ASCII ones. Re-index to pick up
  edges for existing code.
- **`spelunk links check` / `spelunk links list` no longer report a freshly
  indexed linked project as stale.** The cross-project freshness probe resolved
  each linked index's stored (root-relative) file paths against the *linking*
  project's working directory instead of the linked project's own root, so every
  sampled file looked "changed" and the documented CI gate ("`links check` exits
  non-zero if any linked index is stale or missing") false-failed on a clean
  checkout. Both the cross-project probe and the in-project `spelunk check` now
  run through one shared staleness function anchored at the correct project root,
  so they agree: a freshly indexed linked project reports fresh, while a linked
  project with a file modified since indexing still reports stale.
- **A teammate's fetched memory now reaches the default `memory.db` read
  paths, not just the git ref.** Team memory lives on git notes
  (`refs/notes/spelunk`) and is queried through the project's SQLite
  `memory.db`, but nothing imported a fetched teammate's notes into that store
  except `spelunk init`, so their newly-published entries stayed invisible on
  the default read path until a manual re-`init`. Now `memory list`, `memory
  search`, `memory show`, and `context` fold the fetched tracking ref into your
  notes and import it into `memory.db` — but only when the notes ref has moved
  since the last import, gated by an OID marker persisted in `memory.db`, so the
  steady-state read spawns no git subprocess and does no import walk. `memory
  search` and `memory show` previously did not consult the tracking ref at all
  and now do. Reading needs no `--backend git-notes` and no re-`init` (ADR-077).
- **A single `spelunk init` after a clone now hydrates the team's memory.**
  `init` configures the notes fetch refspec and does a one-time, best-effort
  fetch of the notes ref *before* its import pass, so the fresh-clone
  `init → git fetch → init again` dance is gone. Offline `init` still succeeds
  and still configures the refspec; the fetch is non-fatal.
- **`spelunk init` no longer risks silently staging `.spelunk/config.toml`.**
  `init` writes the project slug to `.spelunk/config.toml` and takes no git
  action on it — no `git add`, no commit — and prints a one-line reminder to
  commit the file yourself so the slug travels with the repo. The docs describe
  committing it as an explicit user step (ADR-077 D5).

- **`spelunk memory list --source-ref <sha>` now finds entries anchored to a
  commit by git notes, not just harvested entries.** Every entry written by
  `spelunk memory add` (with the default git-notes write-through) is anchored to
  a commit — its memory note is attached to that commit in `refs/notes/spelunk`
  — but that anchor was recorded only as the git-notes attachment, never in the
  SQLite `source_ref` column (which carries a commit SHA only for *harvested*
  entries). `--source-ref` filtered on that column alone, so it returned zero
  results for every commit whose entries came from `memory add`, even with the
  notes plainly present under `git notes --ref=spelunk show <sha>`. The filter
  now also resolves, from the notes ref, which entries are anchored to the
  requested commit (exact SHA or prefix) and reads the authoritative local rows
  back, so those entries are found while their ids and status stay consistent
  with a plain `memory list`. Harvested `source_ref`-column matches are
  unchanged. On the `--backend git-notes` (and pre-init) path the git-notes
  backend now serves `--source-ref` directly by the same commit anchor instead
  of returning an unsupported-operation error.

- **`spelunk check --format porcelain` now emits only the stable `key=value`
  summary on stdout.** It previously also wrote the human diagnostics — the
  `Server: … ✓` reachability line, the "Active agent sessions" list, and the
  `⚠ Overlap:` warning (with their Unicode glyphs) — to the same stdout stream,
  so a script doing `spelunk check --format porcelain | while read -r line`
  had to filter out prose. Those diagnostics now go to **stderr** in porcelain
  mode, keeping the signal for a human watching the terminal while leaving
  stdout machine-parseable. Text (human) mode is unchanged: the diagnostics
  still print to stdout. Exit codes are unchanged in both modes (0 fresh,
  1 stale).
- **`spelunk explore` is now listed in the top-level `spelunk --help` command
  list.** The agentic-search command was hidden from `--help` whenever no chat
  model was configured, so a user or agent enumerating capabilities from
  `--help` never discovered it — even though `spelunk explore --help` and the
  command itself always worked. It now always lists, like the other commands
  that need infrastructure the user may not have (`sync`, `login`, `org`).
  Running `spelunk explore` without an LLM still fails with the same
  locked-feature message as before; only its visibility in `--help` changed.
- **`spelunk init` now git-ignores the per-run index lock (`index.lock`) and its
  pid sidecar (`index.lock.pid`).** The generated `.spelunk/.gitignore` listed
  the SQLite files and logs but not the lock, so a `git add -A` staged and
  committed `index.lock.pid` — which holds a machine-local process id that churns
  and conflicts across machines. New projects ignore both via an `index.lock*`
  line. Existing projects (whose `.gitignore` init never overwrites) can add it
  manually:

  ```sh
  echo "index.lock*" >> .spelunk/.gitignore
  ```
- **`spelunk plumbing embed` now finds a running `spelunk-server` the same way
  every other server-backed command does.** It reported `requires
  spelunk-server` even while a healthy local server was running and `search
  --mode semantic` / `memory search` used it, because `embed` gated directly on
  a configured `server_url` and skipped the capability-tier resolution the other
  commands run. It now honours the auto-started / auto-discovered loopback
  server (and `SPELUNK_SERVER_URL`), so `echo "text" | spelunk plumbing embed`
  emits the vector whenever a ready server is reachable, restoring the
  `echo … | spelunk plumbing embed --query | spelunk plumbing knn` pipeline. The
  locked-feature error is unchanged when no server is reachable.
- **`spelunk memory harvest` no longer crashes on a repo with fewer than 11
  commits.** The default `HEAD~10..HEAD` range named `HEAD~10`, a commit that
  does not exist in a shallow history, so `git log` aborted with a raw
  `fatal: bad revision 'HEAD~10..HEAD'`. The range is now clamped to the commits
  that actually exist (the most recent `min(10, commit_count)`, root included),
  so harvest works on any repo with at least one commit. A custom `--git-range`
  or `--branch` is passed through unchanged. Harvest also runs its
  LLM-capability precheck before resolving the git range now, matching `spelunk
  explore`: with no LLM configured the actionable locked-feature message is
  shown regardless of repo size, rather than a raw git error on a short history.
- **A partial `[auth]` table in `config.toml` no longer bricks every command.**
  `[auth]` is login-managed, but `--org` is an optional scoping flag and
  hand-editing the config is a documented workflow, so a login without an org
  (no `org_id`) or a trimmed table left the CLI unable to run anything —
  including commands that need no credentials (`status`, `search`, `context`).
  Every `[auth]` field is now optional: a missing/empty `access_token` reads as
  "not logged in" (no bearer sent), a missing `expires_at` as expired, and a
  missing `org_id` applies no scoping, instead of a hard parse error.
- **A `config.toml` that fails to parse now names the file, the offending key,
  and the remedy** instead of a bare, unactionable `Error: parsing config.toml`.
  An unrecognised `mode` value names the bad value and lists the valid modes
  (`offline`, `local_first`, `cloud_first`), matching the `SPELUNK_MODE`
  message.
- **`spelunk search --mode semantic` (and `--mode hybrid`) now fail with an
  actionable error when no server is reachable, instead of silently reporting
  "No results found." and exiting 0.** These modes need a server to embed the
  query, so with none reachable (including under `SPELUNK_NO_SERVER=1`) they now
  emit the same locked-feature error as the other inference-backed commands.
  The default `auto` mode is unchanged: it still announces its degradation and
  falls back to ast-grep.
- **`spelunk memory add --relates-to <id>` now records the relationship.** The
  flag was accepted and the entry stored, but it was never wired to the edge
  layer, so no `relates_to` edge was written and neither `memory graph` nor
  `memory show` showed any link from either side. It now creates a `relates_to`
  edge that is visible from both entries, and — unlike `--supersedes` — archives
  neither of them (a relates_to link is non-superseding). A `--relates-to`
  pointing at an id that doesn't exist is now rejected before anything is
  written, rather than storing an entry with a dangling link.
- **`spelunk memory add --kind` now rejects an unknown kind instead of silently
  storing it.** Previously any string was accepted, so a typo (`--kind
  decisions`, `--kind desicion`) stored an entry with no retrieval path. The
  default (`note`) and every valid kind are unaffected.
- **`spelunk search --mode text` now scores query words as independent terms
  instead of matching the whole query as a contiguous phrase.** A multi-word
  query ranks chunks that contain the terms in **any order**, and a chunk
  containing more of the terms ranks above one containing fewer (BM25
  bag-of-words, as documented). Previously the raw query was passed to FTS5 as a
  single quoted phrase, so word order alone decided whether there was a hit —
  e.g. `leaky bucket` matched a chunk but `bucket leaky` returned nothing.
  Matching stays case-insensitive and unstemmed (`bursts` matches `bursts`, not
  `burst`), following the FTS tokenizer.
- **`spelunk logout --server <url>` no longer signs you out of spelunk.cloud.**
- **`spelunk memory list --as-of` and `spelunk memory search --as-of` now
  reconstruct the past correctly, without needing `--archived`.** A
  point-in-time query asks "what was the state of memory at instant T", so it
  must return every entry that was live at T regardless of its status today.
  Two defects broke that. An entry superseded or archived *after* T was hidden
  unless you also passed `--archived`, so the then-current decision went
  missing from the very query meant to surface it. And an entry created with no
  explicit `--valid-at` stored a NULL validity start, which the filter read as
  "valid since forever", so entries created *after* T still appeared in queries
  about the past. The as-of window is now exactly `valid_at <= T AND
  (invalid_at IS NULL OR invalid_at > T)`, with a missing `valid_at` defaulting
  to the entry's creation time, evaluated independently of archived status
  across list, text, semantic, and hybrid search. `--archived` again controls
  only the current-state view, orthogonal to `--as-of`.
- **`spelunk memory timeline <topic>` now filters by the topic instead of
  dumping the whole store.** The topic argument was ignored: every query
  returned every entry (a nonsense topic returned the same set as a real one),
  because the local path fetched the nearest-neighbour set sized to `--limit`
  and, for any store smaller than the limit, that was simply everything. Timeline
  now routes the topic through the same no-server full-text path as `memory
  search --mode text`, so a topic returns only its related entries and an
  unrelated topic returns none — still sorted ascending by `valid_at`, and still
  including superseded/archived entries so you can see how understanding evolved.
  As a bonus, `memory timeline` no longer needs a running inference server: it
  matches on text, not embeddings.

## [0.9.6] — 2026-07-31

### Added

- **A configurable LLM endpoint and a place to keep its credential.** You can now set `llm_url` and `llm_model` in
  `~/.config/spelunk/config.toml`, override either with `SPELUNK_LLM_URL` /
  `SPELUNK_LLM_MODEL` or per launch with `spelunk server start
  --llm-url/--llm-model`, and store the endpoint's credential with `spelunk auth
  set-key --llm` (read from stdin or a prompt, kept in the OS secret store,
  overridable with `SPELUNK_LLM_KEY`).

  `spelunk-server` gained `--llm-key` and `--llm-key-file` alongside
  `SPELUNK_LLM_KEY`, sends a resolved credential as a bearer token upstream, and
  refuses to start when a credential is configured against a plaintext `http://`
  endpoint on a non-loopback host, naming the URL. That check applies only when a
  credential is present, so an existing keyless LAN endpoint (LM Studio or Ollama
  on your network) keeps working exactly as before, and an endpoint with no
  credential is still sent no `Authorization` header at all.

- **`mode = "cloud_first"` now serves memory against the hosted API, not only a
  self-hosted spelunk-server.**

  `project_id` may be a slug or a UUID against either peer for every memory
  operation, including `spelunk memory show` and `spelunk memory archive`.

### Changed

- **A memory entry's id is now an opaque token rather than an integer.** The
  local store and a self-hosted server number entries sequentially; the hosted
  API identifies them by UUID, which no integer can carry. Ids are therefore
  passed through as opaque values and narrowed back to an integer only by the
  stores that have one. Commands that take an id accept whichever form the
  project's own store uses, and an id from the wrong kind of store now says so
  instead of reporting the entry as missing.

  `--format json` output is unchanged for existing projects: a numeric id still
  serializes as a JSON number, and only a non-numeric id serializes as a string.

### Fixed

- **`spelunk index` no longer skips every chunk summary, and says something you
  can act on when it genuinely cannot make one.** Summaries were gated on
  configuration that had nothing to do with whether an LLM was reachable, so
  the summary pass was effectively off for everyone. LLM availability is now
  decided by what your server actually reports, not by your config: if you've
  set `llm_url` but your local server isn't currently serving an LLM, spelunk
  asks you to restart it rather than silently falling back to a remote one.
  `spelunk explore` and `spelunk memory harvest` share the same fix. Summaries
  stay optional and `index` still exits 0 (`--no-summaries` silences the
  notice); `explore` and `memory harvest` now fail loudly instead of silently
  doing nothing.

- **`spelunk server start` no longer blames your firewall for a daemon that
  refused to start.** It waited out the full 30-second liveness timeout and then
  suggested a firewall whatever had gone wrong, including when the daemon had
  already exited over its own configuration. It now notices the process is gone,
  says so immediately, and points at the log; the firewall suggestion is kept for
  the case it actually describes, a daemon still running and still not answering.

- **A written stability contract, [docs/stability.md](docs/stability.md), and
  tests that enforce it.** Until now the plumbing JSONL schemas were held stable
  by convention alone, and nothing stated which config keys, flags, exit codes,
  or on-disk formats you may rely on across versions. The contract now declares,
  per surface, whether it is stable (semver-bound, additive-only), best-effort,
  or internal, covering CLI commands and flags, plumbing JSONL fields, the `/v1/`
  HTTP API, `config.toml` keys, and the on-disk stores
  (`index.db`/`memory.db` migrations, `registry.db`, git-notes
  `schema_version`, and the `.spelunk/` layout). For config it also freezes
  *which file* a key may be set in, which is not the same question as whether
  the key is supported: `server_url` is ignored in the personal global config
  and `server_key` is ignored in the checked-in project config, both
  deliberately. It is equally explicit about what is *not* stable:
  human-readable porcelain text, log and diagnostic output, and the internal
  crate APIs. The structured `--format json`/`jsonl` modes of porcelain
  commands are a third category, called out separately: `spelunk status
  --format json` is stable for its core fields, and every other `--format`
  mode is best-effort. A deprecation policy (alias, then warn while the alias
  lives, then remove) sets the sequence for future changes; the removed
  `memory_server_url` key is documented as the precedent, including where it
  fell short of it. Enforcement is real, not aspirational: a committed golden schema
  covers every plumbing command's JSONL output, so adding a field passes while
  removing, renaming, or retyping one fails; exit codes 0/1/2 are asserted per
  command, including that exit 2 leaves stdout empty; a guard derived from the
  CLI's own help refuses to let a new plumbing command ship without a declared
  schema; and the checker itself is tested, so it cannot pass by accepting
  everything.

- **`spelunk-server --model-dir <PATH>` (or `SPELUNK_MODEL_DIR`) loads the
  bundled F2LLM-v2-330M embedder from a pre-provisioned local directory, with
  zero network access.** For hosts with no route to `huggingface.co` (an
  air-gapped network, a strict corp firewall, a build image with no egress),
  the previous online-only Hugging Face Hub fetch had no fallback. Provision
  the flat directory (the GGUF plus `tokenizer.json`; `config.json` is
  optional) on a connected machine first and transfer it over; see [Server
  setup → Air-gapped / no-egress install](docs/server-setup.md#air-gapped--no-egress-install)
  for the full fetch-and-transfer procedure. Unset by default, so the online
  path is unchanged for the common case.

- **A version-skew support policy, [docs/version-skew.md](docs/version-skew.md),
  and a CI job that runs two real binaries against each other.** A new `version-skew` 
  workflow puts the current build and the previous release on a socket together in 
  both directions and drives the full memory flow (add, list, search, push, sync, and 
  pull into a fresh checkout), verifying each downloaded release asset against its 
  published digest before running it.

### Removed

- **`SPELUNK_NO_SLUG_CACHE` no longer does anything, and
  `.spelunk/cloud-project-id.lock` is no longer written or read.** Both existed
  only to serve the slug-to-UUID translation removed under Fixed below. The lock
  file was deliberately left out of `.spelunk/.gitignore` so a team would share
  one resolved identity, so a repo that reached the hosted API this way now
  carries a tracked file that means nothing. Delete it whenever you like:
  nothing reads it, nothing regenerates it, and there is no migration to run,
  because the resolver and the passthrough that replaced it key on the same
  org-unique slug and select the same project.
- **`spelunk-server --embedding-url` / `SPELUNK_EMBEDDING_URL` and the deprecated
  `--embedding-model` / `SPELUNK_EMBEDDING_MODEL` no longer exist.** The
  embedding model is pinned product-wide to the bundled native embedder
  (F2LLM-v2-330M@896); there is no longer any way to relocate where embeddings
  are computed, only where LLM inference runs (`--llm-url` / `--llm-model` are
  unaffected). Starting the server with either removed flag now fails with
  clap's unknown-argument error instead of being accepted or silently ignored;
  the corresponding environment variables have no effect. The CLI's
  `embedding_model` config key is also removed: `spelunk plumbing embed` now
  always reports the pinned model id instead of a config-configurable label.
  Existing `config.toml` files that still carry `embedding_model` continue to
  parse unchanged (the key is silently ignored, same as other pruned keys).
  This is a breaking change to `spelunk-server`'s CLI surface, shipped pre-1.0.

### Fixed

- **One unreadable field in a server's health response no longer costs you
  every capability that server advertised.** The CLI reads `GET /v1/health` to
  learn what a server can do. The guarantee is now that no single field can take
  the whole body down, and it is enforced by a test that mutates every member 
  of a recorded server response in turn.

- **`spelunk memory push` and `spelunk sync` now embed what they push, so a
  pushed entry stays findable by `spelunk memory search` locally.** A push
  shipped entries that had never been embedded (a `memory add` with no embedder
  running, an import, a pulled entry) and left the local `memory.db` exactly as
  it found it. The push reported `created`, and those rows remained invisible to
  semantic `memory search` on your own machine, with nothing saying so and no
  hint that `spelunk memory reindex` was the cure. Both commands now embed every
  entry in the push set that lacks a usable local vector before the batch is
  built, through the same local embedder and the same document text
  `memory reindex` uses, and commit each vector as it completes. Note this
  changes nothing about what travels: `kind`, `title`, and `body` were always
  sent on every push, and the optional vector fields are additive. What changes
  is that the local store is left correct, and that a destination advertising
  `accepts_pushed_vectors` can now store the entry as-is instead of re-embedding
  it. With no local embedder reachable the push still runs to completion exactly
  as before, text-only, and says how many entries went out unembedded and how to
  fix them; the summary line reports how many were embedded locally. Skipped in
  `cloud_first` mode with a `server_url` set, where `memory.db` is not the store
  of record and `memory reindex` does not apply either. Rows that were already
  synced are outside the push set and are not embedded by this change.

- **The local `spelunk-server` no longer goes unreachable while it is
  embedding.** During a `spelunk index` run, `/v1/health` could take seconds
  instead of well under a millisecond, `spelunk server status` reported a
  perfectly healthy server as `(unreachable)`, and unrelated endpoints stopped
  answering too. The liveness probe read the embedder's per-chunk token cap
  through the same lock the embedder holds for a whole batch of forward
  passes, and because that read was synchronous inside an async handler it
  tied up a request-serving thread rather than releasing it, so each
  concurrent probe made the stall worse. The cap is fixed at model load and
  never changes, so it no longer sits behind that lock: liveness probes,
  `spelunk server status`, and any other endpoint now stay responsive for the
  whole index. The `/v1/health` payload is unchanged, `limits.embedder_token_cap`
  included.
- **`mode = "cloud_first"` against a self-hosted team server no longer fails
  before it starts.** With a non-loopback `server_url` and a human
  `project_id`, every memory command tried to translate the slug into an
  internal UUID by calling `GET /v1/projects` first, and then keyed the whole
  session by whatever came back. A self-hosted spelunk-server keys projects by
  the slug itself and answers that endpoint in a different shape, so the
  translation could not succeed and the command died with a parse error before
  touching memory at all. `project_id` is now sent exactly as configured, slug
  or UUID: both a self-hosted server and the hosted cloud API accept either, so
  there was never anything to translate. The documented three-line
  `cloud_first` config works as written.
- **`spelunk index` no longer skips a crash-half-indexed file forever.** If a
  previous run was killed after recording a file's new content hash but
  before writing its chunks, the hash-only skip check treated the file as
  already up to date and never reprocessed it, silently leaving it
  unsearchable until someone thought to pass `--force`. A plain `spelunk
  index` now also checks that the file actually has stored chunks, so it
  self-heals on the very next run.
- **Two `spelunk index` runs on the same project could corrupt the index
  database; a second run now fails cleanly instead.** Racing writes from two
  concurrent `index` processes on the same project could reproducibly corrupt
  `index.db`. A per-project lock now serializes runs: a second `spelunk
  index` started while one is already in progress exits immediately with
  `index already running (pid N), try again once it finishes` rather than
  writing to the database alongside the first run.
- **`spelunk search` and other read-only commands could fail with "database is
  locked" while `spelunk index` was running.** Every database open re-stamped
  a schema-version pragma even when nothing needed migrating, and that stamp
  always opens a write transaction, so a purely read-only command could
  contend for the write lock against a concurrent `index` run. Opening an
  already-current database is now read-only.
- **`spelunk sync` and `spelunk memory pull` no longer silently stop after
  the server's first page of entries.** A pull request never sent an
  explicit page size, so the server applied its own 100-entry default; on a
  first sync into an established project with more than 100 pending
  entries, the command reported success and printed a count, but only the
  first page ever landed locally, with no error and no indication anything
  was missing. Both commands share the same pull path, which now requests
  the server's maximum page size and keeps paginating, applying each page
  as it arrives, until a page comes back short of that size. A large first
  sync or pull now completes fully in one command instead of requiring
  several repeated runs to converge.
- **`spelunk index`'s embed phase now embeds locally in `local_first` mode
  even when a team `server_url` is configured.** Both the foreground (default)
  embed phase and the `--detach-embed` background worker's embedder-readiness
  poll resolved their embedding target from the raw tier-probing functions
  (`get_tier`, `probe_tier_fresh`) instead of the mode-aware
  `get_inference_tier`/`get_inference_tier_fresh`: a `local_first` project
  with an explicit `server_url` sent every embed batch to that `server_url`
  instead of the local, auto-discovered embedder, silently skipping embedding
  when the configured server has no `/index/embed` route. Both call sites now
  route through `get_inference_tier`/`get_inference_tier_fresh`, the same fix
  already applied to `memory add`/`reindex`/`search`/`timeline`/`harvest`/
  `reconcile` and `explore`. `cloud_first` is unchanged. See [ADR-004's
  2026-07-23 amendment](docs/adr/004-unified-memory-storage.md).
- **`spelunk sync` and `spelunk memory push` now succeed on the first sync of a
  project that has never synced before.** A sync round always pulled before
  pushing to avoid shadowing a concurrent write, but on a project that never
  existed server-side yet, the pre-push pull failed with an HTTP 400 against
  the unprovisioned project. On a first sync (detected by checking whether any
  local entry has ever synced), the sequence now reverses: push first to
  provision the project server-side, then run a post-push pull to converge with
  any backlog already on the server. The fix also handles adversarial server
  crashes during first-sync push and concurrent first-sync attempts from
  multiple clients.

### Security

- **A `server_url` whose host only *looks* like loopback no longer clears the
  plaintext-transport guard.** Deciding "is this loopback?" was a prefix test,
  `host.starts_with("127.")`, applied to a string from which URL userinfo had
  never been stripped. Two authority shapes therefore passed as loopback while
  naming somebody else's host: `http://127.0.0.1.evil.example`, where the real
  host is `evil.example` and only the leading label looks like an address, and
  `http://127.0.0.1@evil.example`, where everything before the `@` is a
  credential and the real host is again `evil.example`. Because that predicate
  is what decides whether a bearer token may travel over plaintext `http://`,
  configuring either shape sent the bearer in the clear to a host the operator
  did not intend, at every call site that gates on it: opening a remote memory
  backend, the sync client's keyed constructor, the CLI capability probe, and
  the inference client. The authority is now parsed rather than pattern-matched:
  it ends at the first `/`, `?`, `#` or `\`, userinfo is removed at the last
  `@`, the port and IPv6 brackets are stripped, and the remaining host must be
  `localhost` or an address literal the standard library parses and reports as
  loopback. The backslash is part of that delimiter set because the URL parser
  that opens the connection treats it as a path separator for `http`/`https`,
  so `http://evil.example\@127.0.0.1` names `evil.example`, not loopback.
  One user-visible consequence: only a full dotted quad counts as a loopback
  literal now, so IPv4 spellings that previously rode in on the `127.` prefix
  are rejected, whether they were invalid (`127.999.0.1`), non-canonical
  (`0127.0.0.1`, `127.0.0.001`) or merely abbreviated (`127.1`). Write the
  address out as `127.0.0.1` if you were using one of those. Unchanged, and
  still deliberate: this check does no DNS resolution, so a `/etc/hosts` alias
  that resolves to loopback but isn't spelled as a loopback literal is still
  rejected rather than accepted.

### Internal

- **`memory.db` now opens through the same forward-only, `PRAGMA
  user_version`-gated migration runner `index.db` already uses**, replacing
  ad-hoc re-execution of the schema SQL with idempotency inferred from
  `ALTER TABLE` error strings. A pre-existing store has its version inferred
  once from table/column shape, then only the missing steps run; a store
  stamped with a version newer than the binary supports refuses to open
  instead of mis-running steps. No CLI flag, config key, or user-facing
  behavior changed.

## [0.9.5] — 2026-07-24

### Changed

- **Default chunk-token cap (`MAX_CHUNK_TOKENS`) lowered from 2048 to 512.** A
  quality/performance evaluation across a large gold-query corpus found
  retrieval quality flat across 2048/1024/512/384 while 512 measurably speeds
  up indexing, with second-order costs (vector count, storage) at the new cap
  confirmed immaterial. `index_meta` now also tracks the chunker
  configuration an index was built under, alongside the existing
  `embedding_model`/`embedding_dim` provenance: opening an index that was
  built under a different chunk-token cap prints a warning naming the drift
  and pointing at `spelunk index --force` for a uniform re-index, rather than
  silently leaving unchanged files on their old chunk boundaries forever. A
  normal incremental run still proceeds after the warning: unlike an
  embedding-model mismatch, a chunk-cap change is same-model/same-dimension
  drift, not index corruption.

### Added

- **`spelunk memory reindex` backfills local note embeddings that were never
  minted, so semantic `memory search` can surface those notes again.** A note's
  vector is created only at `memory add` time; if no embedder was reachable
  then, or the store was upgraded across the 768→896 embedding-dimension change
  (which drops the old vectors), the note stays present-but-unembedded with no
  catch-up path and is invisible to semantic KNN (it is still reachable via text
  search, `list`, `timeline`, and `context`). `reindex` re-embeds those notes
  through the same local embed path `memory add` uses, committing each vector as
  it completes so an interrupted run resumes on re-run rather than starting over.
  By default it embeds only active notes missing a vector; `--force` re-embeds
  every active note, replacing any existing vector (useful after a model or
  dimension change); `--include-archived` also covers archived notes; `--dry-run`
  reports the count and writes nothing; and `--format json` emits a
  machine-readable summary. When no embedder is reachable it fails with an
  actionable error and writes nothing. After the 768→896 upgrade drops old
  vectors, memory commands print a one-line notice pointing at `spelunk memory
  reindex` so the recall regression is discoverable without `RUST_LOG`.

- **`local_first` writes now queue and drain automatically; you no longer
  need to run `spelunk sync` by hand in the normal path.** A write (`memory
  add`/`archive`/`supersede`) with a team `server_url` configured still
  commits to `memory.db` and returns immediately with no network call in its
  own path, exactly as before. What's new is what happens next: the entry sits
  in a local outbox (still just the same `memory.db` rows, no new table) until
  a background reconciler drains it. From an interactive terminal session, the
  write opportunistically starts (or reuses) the local `spelunk-server` and
  hands it the outbox to push; that process also holds a live pull connection
  to the team server, so entries recorded elsewhere on the team tend to show
  up locally without an explicit `spelunk sync`. Non-interactive invocations
  (CI, scripts, git hooks) never auto-start a server: the write still commits
  and stays durably queued, and drains on the next interactive session or
  explicit trigger. `spelunk status` now shows a quiet pending-entry count and
  last-synced freshness for `local_first` projects (for example `mode
  local_first  ·  2 pending, last synced 4m ago`; nothing extra when there's
  nothing to report), in text and `--format json` (`sync_pending` /
  `sync_last_synced_at`). `spelunk sync` and the one-way `memory push` /
  `memory pull` still work unchanged for a forced, synchronous reconcile or a
  non-interactive context. See [Team server and sync
  modes](docs/memory.md#team-server-and-sync-modes).

- **`spelunk memory dedupe` collapses duplicate-`entity_id` groups already
  resident in a project's `memory.db`.** A store can already hold rows that
  share the same content identity (`kind`/`title`/`body`) while differing in
  `created_at`, `tags`, `linked_files`, or `status`, for example a decision
  recorded twice by a repeated `memory harvest` run, or one merged in from
  another machine. Opening the store now backfills each row's `entity_id` but
  never deletes an existing row on its own, so this new command is the
  explicit, dry-runnable way to clean duplicates up: `--dry-run` reports
  duplicate groups and does nothing, otherwise the earliest-created row in
  each group survives, the others' `tags` and `linked_files` merge onto it,
  and any `supersedes` edge pointing at a removed row is repointed to the
  survivor. It is a one-time backfill you run when you want to, not a step
  `init` or `memory add` perform for you. See [ADR-068's third
  amendment](docs/adr/068-zero-setup-onboarding-git-notes-memory-fallback.md).

### Fixed

- **`spelunk memory add` no longer crashes on a byte-identical duplicate once
  a project's `entity_id` index has been promoted to UNIQUE.** Previously a
  second `memory add` with the same `kind`/`title`/`body` as an existing
  entry hit `Error: UNIQUE constraint failed: notes.entity_id`. It now reuses
  the existing entry instead, merging the new call's `tags` and
  `linked_files` into it, and prints `Already recorded as [kind] #id: title`
  in place of `Stored [kind] #id: title`. See [ADR-068's fourth
  amendment](docs/adr/068-zero-setup-onboarding-git-notes-memory-fallback.md).
- **A `local_first` project with a team `server_url` configured now embeds
  locally instead of 404ing.** Query and note embedding for `memory add`,
  `memory reindex`, `memory search`, `memory timeline`, `memory harvest`,
  `memory reconcile`, and `explore` used to route to whatever `server_url`
  was configured, even in the default `local_first` mode, where `server_url`
  is only a sync replica and often has no `/index/embed` route at all (e.g.
  spelunk.cloud). The result was a 404 on `memory search`, and a silent,
  unembedded write on `memory add`/`reconcile` (the note still saved, just
  invisible to semantic search). Inference routing now keys off `mode`
  instead of `server_url` presence: `local_first`/`offline` always use the
  local, auto-discovered embedder; `cloud_first` still offloads embedding to
  the server. `memory reindex` additionally rejects `cloud_first` with a
  `server_url` configured, since `memory.db` isn't the store of record
  there. See [ADR-004's 2026-07-23
  amendment](docs/adr/004-unified-memory-storage.md).

- **`spelunk memory push` and `spelunk sync` can now push a real-sized project
  instead of timing out.** Previously a project with more than a few dozen
  entries was pushed in requests bounded by a fixed 30-second client timeout, so
  the push timed out and nothing landed: the server project was never even
  created. The push now splits entries into smaller per-request batches and
  gives each request the longer, inference-class timeout the server needs to
  re-embed them, so a push of hundreds of entries completes and the project is
  created by the first batch. If a batch fails partway through (for example the
  server is briefly overloaded), the push stops at that batch, reports how far
  it got (`Pushed X of Y entries, then stopped: ...`) with a hint to re-run to
  resume, and exits non-zero rather than reading as success. Batches that
  already landed are recorded, so a re-run pushes only the remainder and any
  entry the server already holds comes back skipped, never duplicated.
- **`spelunk sync` / `spelunk memory sync` no longer permanently skips a
  teammate's older memory on a client's first sync.** The pull cursor is
  derived from the newest `remote_id` known locally, and `memory_sync`
  previously pushed before pulling, so a client's own brand-new push became
  that newest id; any teammate content already on the server that this
  client had never pulled was silently and permanently skipped, with no
  error. This mainly hit a new team member's very first sync on a project
  with prior history, but the same shape could also shadow a teammate's push
  landing in the narrow window between a client's own pull and its own push
  on any sync round. `memory_sync` now pulls, pushes, then pulls a second
  time reusing the pre-push cursor, so a concurrent teammate push in that
  window is caught within the same sync call or, at the latest, the next
  one. No command, flag, or output format changed.

### Changed

- **Chunk re-windowing is now token-aware, and windowed chunks keep their identity.**
  Oversized code nodes (and unsupported-language fallback content) were previously
  re-split into a fixed 120-line window regardless of token count, so `MAX_CHUNK_TOKENS`
  decided only *when* to re-window, never *how big* the result was — long-line
  machine-generated content could still produce single windows of 10,000+ tokens.
  Windows now accumulate whole lines up to the `MAX_CHUNK_TOKENS` budget (with a
  single over-budget line becoming its own window, so the split always makes forward
  progress), keeping ~12.5% token overlap between adjacent windows. Each window also
  now carries its source node's name, docstring, and enclosing scope, so a re-windowed
  function embeds with its symbol identity instead of `title: none`. Existing indexes
  remain valid — this changes neither the DB schema nor the embedding format — but a
  `spelunk index --force` re-index is recommended (not required) to pick up the
  improved chunk shape on already-indexed repos with large, long-line files.
### Security

- **Self-hosted server bearer credentials are now scoped per server, not
  global.** Previously a single flat `server_key` served every configured
  `server_url`, so a developer working on two projects that each point at a
  different self-hosted server (the topology [ADR-056](docs/adr/056-oss-server-tenancy-model.md)
  recommends over multi-tenancy) had one key slot for two servers, with
  whichever key lost getting 401s. The bearer for a request is now resolved
  per the target server's origin, so keys for distinct self-hosted servers
  coexist without collision or manual env-var juggling. A pre-existing flat
  key migrates into the new per-server store automatically the first time
  it's needed. See [ADR-071](docs/adr/071-per-server-client-bearer-scoping.md).

### Added

- **`spelunk auth set-key --server <url>` and `spelunk auth list-servers`.**
  `set-key` stores a bearer key for a self-hosted server, scoped to its
  origin; the key is read from stdin or an interactive prompt only, never
  from a flag or positional argument, so it never lands in shell history or
  `ps` output. `list-servers` prints the origins with a stored key (never the
  key material itself). (ADR-071)
- **`spelunk logout --servers` / `spelunk logout --server <url>`.** Clearing
  self-hosted server keys is now an explicit, separate action from clearing
  the spelunk.cloud login: bare `spelunk logout` clears only the `[auth]`
  token pair, so recovering from a broken cloud login no longer has the side
  effect of deleting server keys used on other projects. (ADR-071 D3)

### Removed

- **`server_key` is no longer read from a project's committed
  `.spelunk/config.toml`.** That field let a plaintext credential live in a
  file the docs otherwise say is safe to commit. It is now silently ignored
  (matching the earlier `memory_server_*` alias removal precedent) rather
  than migrated or warned about; use `spelunk auth set-key --server <url>` to
  store the key per-developer instead. If a key was ever committed under the
  old model, treat it as compromised: rotate it on the server and re-set it
  on every machine that used the old one. (ADR-071 D4)

### Fixed

- **`spelunk-server` no longer computes an abandoned embed batch to
  completion.** The native embedder moved a whole `/index/embed` batch into a
  detached blocking task, so when the CLI's client gave up on a slow batch
  (`batch_timeout`) or the server's own 1800s timeout fired first, the
  connection closed but the embedding work kept running on the GPU/CPU for a
  result nobody would ever read, measured at ~37% of one run's GPU time spent
  on a single abandoned batch. A client disconnect or server-side timeout now
  cancels the in-flight batch (checked when a queued batch reaches the front
  of the embedder's lock, between sub-batches, and per chunk on the
  sequential path), and the abandonment is logged with the chunk count
  completed. No request/response contract change. (#631)
- **`memory add --supersedes OLD` against an already-archived OLD no longer
  writes conflicting git-notes carrier records.** The SQL layer's archive-OLD
  update already silently no-ops when OLD is not active, but the CLI never
  checked that outcome before appending a state-update record to the git-notes
  carrier, so running `--supersedes OLD` twice with two different successors
  left two conflicting `archived` records for OLD's entity, each naming a
  different successor — and the read-time fold picked between them by
  lexicographic string comparison, not recency, so the wrong successor could
  silently display. `--supersedes` now checks OLD is active *before* any
  write (SQLite or git-notes), on both storage paths, and fails with the same
  "No active memory entry with id `<old-id>` (old)." error `memory supersede`
  already gives for the same case — this is a deliberate behavior change: the
  command used to succeed against a stale OLD, and now it errors instead.
  Fold-time resolution of any conflicting records written before this fix
  (or by a lost cross-machine race) is also hardened: `superseded_by_entity_id`
  now resolves to the record with the greatest `created_at`, not the smallest
  `entity_id` string. (ADR-068 E4/E5)
- **Indexing no longer drops the docstring from a documented Rust item
  followed by an attribute, or a documented Python function/class wrapped in
  a decorator.** `preceding_comment`'s walk back over sibling nodes stopped
  at the first non-whitespace node it found, so a Rust `#[derive(...)]` /
  `#[async_trait]` attribute (a real sibling sitting between the item and
  its doc comment) or a Python `@decorator` (which tree-sitter-python wraps
  together with the definition in one `decorated_definition` node) made the
  walk land on the attribute or wrapper instead of the comment, so no
  docstring was captured at all, silently. This affects retrieval quality,
  since a chunk's docstring is part of what gets embedded and searched
  (`Chunk::embedding_text()`); attributed/decorated items are common in
  async Rust services and in most idiomatic Python. The walk now skips
  `attribute_item` siblings (Rust) and starts from the enclosing
  `decorated_definition` when present (Python), so the docstring is
  captured as before. TypeScript decorators, Java annotations, and every
  other currently supported language with similar syntax (PHP, Kotlin,
  Swift, C#, C++, C) attach it as a child of the item in their grammars and
  were confirmed unaffected. No schema or embedding-format change; run
  `spelunk index --force` to recover docstrings on already-indexed
  attributed/decorated items.
- **`spelunk index` no longer treats a failure to connect to the local
  server the same as a slow request, and no longer lets one poison later
  batch sizing.** A batch that failed because the client couldn't open a
  TCP connection at all (the server momentarily unreachable, not just slow
  to respond) was previously classified the same as a request that ran out
  of its time budget: the client halved the batch size and folded the
  failed attempt's elapsed time into its running throughput estimate, even
  though a connect failure carries no signal about batch sizing or
  embedding speed. Connect failures are now classified separately and
  retried at the same batch size with a bounded backoff (five attempts,
  5s to 180s) instead of shrinking, and no longer feed the rate estimate;
  once those retries are exhausted, indexing falls back to the existing
  manual re-run path unchanged.
- **`spelunk sync` now actually pulls teammates' entries for a client that
  has already pushed or synced before.** The team server's batch-push
  endpoint acknowledged each pushed entry with its raw database row id
  (e.g. `"1"`) instead of its `sync_id`, and the CLI stores whatever id it
  is acknowledged with as that entry's `remote_id`, then computes its next
  pull cursor as the greatest `remote_id` it has on file. A small integer
  string sorts lexically after every real `sync_id` (a UUIDv7, which starts
  with a hex timestamp), so once a client had pushed anything at all, its
  next pull matched nothing server-side even when teammates had added newer
  entries. A fresh client, whose `remote_id` starts unset, never hit this,
  which is why the fresh-clone case always looked fine while day-to-day team
  sync quietly stopped receiving updates after the first push. The server
  now acknowledges pushes with the same `sync_id` `/memory/since` cursors
  on, so an established client's pull cursor advances correctly.

### Changed

- **`spelunk index` skips generated, vendored, and minified files.** Build output,
  vendored dependencies, and minified assets are no longer parsed, chunked, or
  embedded, so the index reflects source you wrote rather than machine-generated files.
- **Linux release binaries now target glibc 2.31 as the support floor.** Builds
  run in a `debian:11` (Bullseye) container to ensure compatibility with Debian
  11+ and Ubuntu 20.04+. Previously, releases silently required glibc 2.39
  (Ubuntu 24.04-era), causing crashes on older distros. The `.deb` package
  declares `libdbus-1-3` as a dependency; tarball users on minimal images must
  install `libdbus-1-3` separately.
- **`spelunk init` starts the server before indexing and detaches the embedding pass.**
  On a fresh install, the prompt now returns after parsing, with embeddings arriving in the background. A detached worker
  polls the embedder readiness and runs the embed phase, resumable by re-running
  `spelunk index`. The server is auto-started before parsing begins (rather than after),
  and a not-yet-ready embedder is a transient condition to wait on rather than a
  terminal reason to skip the embed pass. (ADR-070 D1, D2)
- **Search over a warming index emits coverage-gated notices.** When KNN search runs
  over an incompletely-embedded corpus, a one-line stderr notice names the coverage
  percentage and its shape ("front-loaded by importance and recency"). In `auto` mode on zero
  coverage, search falls back to ast-grep with a notice naming embeddings as building
  in the background; in explicit `semantic`/`hybrid` mode, zero coverage produces an
  actionable error naming the resume command instead of "No results found." Partial
  coverage results stay served (KNN order-independence + useful prefix) and are labelled
  accordingly. (ADR-070 D3)
- **`spelunk status` reports the embed worker's recorded liveness and token-weighted
  progress.** A live background worker triggers "Embedding in progress" (not a guess
  from embedded counts); no live worker + pending work prints "Embedding incomplete"
  plus the resume command. Coverage (chunks embedded / total chunks) and progress
  (percentage of work done, measured by token weight) are two separately-named measures,
  and a measured-this-run ETA derives from the worker's recorded baseline (never cached
  across runs). The old hedging parenthetical is gone. JSON status gains
  `embedding_pending`, `embed_worker_alive`, and `embed_tokens` fields. (ADR-070 D4, D6)

- **Memory entries are now identified by their content.** An entry's canonical
  identity is a SHA-256 over exactly its `kind`, `title`, and `body`, so the
  same decision recorded independently on two machines converges on one
  identity, with no server and no coordination. Previously the identity that
  travelled in `refs/notes/spelunk` was the local SQLite row number, which
  `spelunk init` renumbers and each machine assigns independently, so two
  different entries could both be stamped `"id":1` in one notes ref. Supersede
  edges now resolve by that content identity and survive a renumber. The
  numeric `id` in `memory list` output is unchanged and remains a local row
  number. (ADR-068)
- **Two entries that differ only in when they were recorded are now one entry.**
  `memory reconcile` and the `spelunk init` git-notes import previously folded
  the creation time into their dedup key, so identical text recorded twice at
  different times stayed as two entries. A creation time cannot be reproduced by
  a second party recording the same decision, so it is no longer part of the
  key: those entries now collapse into one on import. If you recorded the same
  decision more than once, expect to see a single entry after importing.
- **Two entries that differ only in their tags or linked files are now one
  entry, and their tags and linked files are merged.** `tags` and `linked_files`
  are likewise out of the dedup key, so entries with identical text but
  different tags collapse on import rather than staying separate. The survivor
  carries the union: values are added, never removed, so nothing recorded on a
  collapsed copy is lost. `memory reconcile --format json` reports the number of
  folded rows as a new `collapsed_duplicates` key, and its counts partition the
  source rows exactly
  (`candidates == already_present + collapsed_duplicates + imported`).
- **`spelunk graph <symbol>` no longer implies a heavily-referenced symbol is
  unused.** The live structural scan matches only bare call syntax
  (`symbol(...)`), so class, constant, association, and receiver-method
  references never appear in it. When the scan finds no call sites but the
  directory has scannable source, the message now reads "No call-site
  invocations of '<symbol>' found (live structural scan matches '<symbol>(...)'
  calls only)." and points at `spelunk init`, whose index carries the
  imports/extends/implements edges the call-scan cannot see. The empty/umbrella
  directory message is unchanged and still never suggests `init`.
- **`graph`, `chunks`, `explore`, and `check` no longer read the machine-global
  index (`~/.config/spelunk/index.db`) from an un-`init`'d directory**, extending
  the ADR-067 fail-closed posture to these read-only display commands (previously
  they resolved via the legacy `open_project_db` fallback and could surface
  cross-project data from the global store). In an uninitialized directory,
  `graph <symbol>` now degrades to a live ast-grep scan; `graph <file-path>`,
  `chunks`, `explore`, and `check` refuse with `no spelunk project here. Run
  'spelunk init' first`. Initialized projects and explicit `--db` are unaffected.
- **`spelunk graph <symbol>` gives unambiguous zero-result messages.** When the
  index holds graph data but the symbol has none, it points to `spelunk graph
  <symbol> --live` for a structural scan; when the index holds no graph data at
  all, it auto-falls-back to that live scan (matching the no-project behaviour).
  The live scan distinguishes an empty or umbrella directory ("No scannable
  source files under this directory") from a genuine no-call result (see the
  reworded call-site message above), and never suggests `spelunk init` for an
  empty/umbrella tree.
- **`spelunk init` no longer writes a `CLAUDE.md` into the target repository.**
  Users who want an agent guide should manually copy `docs/examples/AGENT.md`
  and rename it to `CLAUDE.md` or `AGENT.md` as needed.
- **`spelunk index` embed-phase messages drop the pre-1.0 "older build /
  upgrade the server" advice.** The three user-facing embed messages keep their
  actionable guidance (the request-budget hint and the conservative-budget
  fallback) but no longer suggest the server may be a legacy build or tell users
  to upgrade or restart it. The request-budget fallback behaviour is unchanged.

### Added

- **`spelunk init` imports existing git-notes memory into the project
  `memory.db`.** When the enclosing repo already carries entries on
  `refs/notes/spelunk` (for example a fresh clone whose memory travels in git
  notes), `init` imports every entry not already present into the local
  `memory.db`, so `spelunk memory list` reflects the repo's recorded history.
  The import is idempotent (re-running `init` imports nothing) and carries no
  embeddings; it prints `Memory:  imported N entries from git notes` when at
  least one entry is imported. (ADR-068)
- **`memory add` and `memory list` work before `init`** when inside a git
  repository, so you can record and list decisions without running `spelunk init`
  first. Pre-`init` entries ride the same git-notes write-through that already
  runs after `init`, landing in `refs/notes/spelunk` on HEAD with an identical
  record shape; there is no local SQLite store until you `spelunk init`. The
  notes stay on the local machine unless you push the ref. `memory search`
  remains gated to initialized projects. (ADR-068)
- **`spelunk init` now configures the `origin` fetch refspec for `refs/notes/spelunk`**,
  so project memory notes travel automatically on `git fetch`. When init detects
  an `origin` remote, it adds the refspec to the fetch config (idempotently) and
  names the command that publishes memory back (`spelunk hooks install
  --pre-push`, below). Teammates then run `spelunk init` in their
  clones (or manually add the same refspec) and `git fetch` to receive notes. In
  projects without an `origin`, init prints the exact git commands to run later
  when the remote is added. See [docs/memory.md](#sharing-memory-across-clones-via-git-notes).
  (ADR-068; the refspec value is corrected by ADR-069, see Fixed)
- **`spelunk hooks install --pre-push` publishes your memory on `git push`.** The
  hook fetches the remote's `refs/notes/spelunk`, merges it into yours with
  `cat_sort_uniq` (a union, so neither side's entries are dropped), and pushes
  the result to the named remote you are pushing to, so decisions travel with the
  code they describe. Publishing is **opt-in**: your memory stays local until you
  install it, and `spelunk init` now says so and names the command. Reading a
  teammate's memory needs no opt-in and is unchanged. Publishing follows the
  remote's *name*: a push that spells out a URL (`git push https://… main`) has no
  name to resolve, so it pushes code without publishing memory and a later `git
  push origin` publishes it.

  Publishing is tied to `git push` because that is the only moment that reliably
  coincides with "this code is being shared". An entry recorded against a commit
  you have not pushed can reach the remote while the commit does not, leaving the
  note unresolvable in a fresh clone: it is on the remote, and nobody ever sees
  it. That is what the old per-change manual push hint produced whenever you
  recorded a decision before pushing the commit it describes, which is the normal
  order of work; `init` no longer advertises it. Pushing the notes ref by hand
  still works as a no-hook fallback, after you have pushed the commits.

  The hook **never blocks your push**: on a publish failure it warns on stderr
  and exits 0, so a failed publish cannot cost you your code push. It retries a
  lost race up to three times and never force-pushes. The one case that does stop
  your push is spelunk itself being gone: the hook records the absolute path of
  the binary that installed it rather than looking `spelunk` up on `PATH`, so it
  keeps working under GUI git clients (which on macOS inherit their environment
  from launchd, not your shell profile). If you move or reinstall spelunk, re-run
  the install command to re-resolve the path. It never overwrites a pre-push hook
  it did not write, and `spelunk hooks uninstall` removes it. (ADR-069 D1/D3/D7)
- **`spelunk plumbing publish-notes`** is the flow behind that hook, which is a
  shim around it. It is the first plumbing subcommand that **writes** and
  performs **network I/O**, so the namespace is no longer read-only by
  construction; anything that assumed so needs to account for it.
  `--best-effort` downgrades a publish failure to a stderr warning and exit 0.
  (ADR-069 D7)
- **Native in-process HTTPS for `spelunk-server`** via `--tls-cert`/`--tls-key`
  (env `SPELUNK_SERVER_TLS_CERT`/`SPELUNK_SERVER_TLS_KEY`), both-or-neither. The
  server terminates TLS itself, so a team/remote deployment is a routable
  `--host` plus the TLS flags and an API key, with nothing in front. A
  non-loopback bind is now allowed only with both TLS and an API key set;
  loopback binds are unchanged. (ADR-066)
- **CLI trust for internal-CA / self-signed team-server certificates.** Point the
  CLI at a PEM CA bundle with `SPELUNK_SERVER_CA` (env) or `server_ca` in
  `.spelunk/config.toml` (env overrides config) to trust a `server_url` whose
  certificate is signed by an internal or self-signed CA. The bundle is added as a
  trust anchor on top of the built-in roots; TLS verification stays on and there
  is no insecure/disable switch. The trusted certificate must be a proper CA (or a
  CA-to-leaf chain); a bare self-signed `openssl req -x509` end-entity certificate
  is rejected. (ADR-066)

### Removed

- **Deprecated `memory_server_url` / `memory_server_key` config keys (and the
  `SPELUNK_MEMORY_SERVER_URL` env var).** These were backward-compat aliases for
  `server_url` / `server_key`; use those (and `SPELUNK_SERVER_URL`) instead. An
  old config that still carries the deprecated keys continues to load, but the
  keys are now silently ignored rather than mapped.

### Fixed

- **`spelunk hooks install` honors `core.hooksPath`.** Hooks were previously written
  to `.git/hooks` even when `core.hooksPath` pointed elsewhere, so git never ran them
  while `init` reported them installed. Install now resolves the hooks directory via
  git and writes where git will invoke them.
- **A TLS certificate failure against a configured `server_url` no longer
  reports "unreachable".** When the capability probe's TLS handshake failed,
  the WARN printed only reqwest's flattened top-level message ("error sending
  request for url (...)") and `spelunk status`/`spelunk check` showed
  `[unreachable]`, exactly as if the server were down, even though it was
  reachable and only certificate trust had failed. The probe now walks the
  full error chain and prints it, special-cases the classic self-hosting.md
  client-trust traps (a CA:TRUE certificate served as its own leaf, or a
  `server_ca` file that is the server's leaf rather than the issuing CA), and
  distinguishes the two failure modes in output: `[unreachable]` for a
  TCP/connect-level miss (refused, timed out), `[tls: <cause>]` for a
  connection that reached the server but failed TLS trust.
- **`spelunk memory push` now works against OSS team servers.** The batch-push
  endpoint (`POST /v1/projects/{id}/memory/batch`) was previously available only
  on cloud-api, so `spelunk memory push` returned 405 Method Not Allowed against
  a self-hosted `spelunk-server`. The OSS team server now implements the same
  endpoint with idempotent re-push on `external_id`, enabling push-only workflows.
- **`spelunk sync`'s pull leg now works against OSS team servers.** The
  pull half spoke a different wire format than the OSS server's
  `/memory/since` endpoint understood (a UUID cursor and an `{entries,
  count}` envelope vs. the endpoint's timestamp-only, bare-array contract),
  so `spelunk sync` could push but not pull against a self-hosted
  `spelunk-server`. `/memory/since` now accepts an optional `since_id` cursor
  alongside the existing `t` timestamp parameter and returns the matching
  envelope shape when it is used; `spelunk memory since` (which still uses
  `t`) is unaffected.
- **`spelunk memory watch` no longer panics when only an auto-discovered
  loopback server is available.** `require_tier1` gates on the probed
  capability tier, which also passes for an auto-discovered inference-only
  loopback server (ADR-004) whose `server_url` is unset; `memory watch`
  unwrapped that case with `.expect("require_tier1 passed")` and crashed
  instead of reporting the missing configuration. It now returns the same
  actionable "requires `server_url` to be configured" error that `memory
  push` and `sync` already use for this case.
- **`spelunk memory push` and `spelunk memory sync` no longer claim to have
  pushed entries that were never sent, never durably persisted, or that the
  server rejected.** Three bugs in the same push path could each make "Done.
  Pushed N entries" (or `sync`'s equivalent) print when nothing meaningful had
  happened. The reported count was the number of sync-eligible rows rather
  than the rows actually included in the batch request, so a push where every
  row was already synced — no HTTP request sent at all — still printed
  "Pushed N entries." Separately, a batch item was stamped with the local
  `remote_id` that permanently excludes a row from all future pushes as soon
  as the server's response carried an id for it, without checking the item's
  own reported status first; a response that returned an id for an entry it
  had not actually persisted would silently and permanently take that row out
  of every future retry. And the summary trusted the server's aggregate
  `created`/`skipped` counters instead of the authoritative per-item
  `results[]` list, so a batch whose aggregate counters understated what
  happened (observed: a server reporting `created: 0` for entries it had in
  fact persisted) was reported exactly as understated, not as what actually
  happened. All three are fixed: the reported count now reflects rows
  actually sent, a row is only stamped as synced when its own status
  affirmatively means the server durably has it, and created/skipped/failed
  counts are reconciled from per-item results (the aggregate counters are used
  only as a fallback when the server sends no per-item detail at all). A push
  where nothing was sent now reads "Nothing to push — N entries already
  synced." instead of implying work was done, and a batch with a partial
  failure reports the real successes and failures instead of masking them.
  A fourth gap in the same path is also fixed: a push where every attempted
  entry failed (nothing created, nothing skipped) previously still printed
  "Done."/"Sync complete." with a failed count appended and exited 0. Both
  commands now treat a total failure as a hard error — the message leads
  with "Push failed"/"Sync failed" instead of success framing, and the
  process exits non-zero, so a caller checking the exit code or skimming
  for "Done" can no longer mistake a fully-failed batch for a completed one.
  `spelunk sync`'s pull step still runs and its results are still reported
  even when the push half fails outright.
- **Concurrent memory writes can no longer silently erase each other's
  entries.** The git-notes write path is a read-modify-write of the note on
  `HEAD`, and nothing serialized it: two simultaneous `memory add` commands
  could read the same note body, and the later write-back dropped the earlier
  writer's entry, with both exiting 0. Worse, a writer treated *any* failure to
  read the existing note as "no note yet", so one transient git failure inside
  the write rewrote the whole note as just that writer's line, erasing every
  prior entry (observed live on Windows CI, where it wiped 6 of 8 concurrent
  entries). Three changes close this. Writes are now serialized end to end by a
  cross-process lock file in the git common dir, one lock shared by all
  worktrees because worktrees share the notes ref. A failed note read is
  retried briefly and then fails the writer, rather than being mistaken for an
  empty note. And a writer that cannot take the lock within its 5-second wait
  fails with an error naming the lock file and telling you to retry; it never
  writes unlocked. Many concurrent writers on a slow machine can exceed that
  wait legitimately: every entry already written is intact, and retrying the
  failed command is the remedy. What the failure looks like depends on the
  store: after `spelunk init` the entry is already safe in `memory.db`, so
  `memory add` exits 0 and prints `Warning: entry stored locally, but the
  git-notes carry failed, so it will not travel with the repo: …` on stderr
  (previously a failed carry was logged where nobody saw it); before `init`,
  and with `--backend git-notes`, git notes is the primary store, so `memory
  add` fails. On the rare filesystem where the lock file cannot be created at
  all, the write-through proceeds unserialized and prints `Warning: wrote to
  git notes without the cross-process lock …` on stderr: concurrent writes
  there can still lose entries, and the warning says so. (#185, #632; ADR-069
  D6/D8)
- **A decision recorded independently on two machines now lists once, not twice.**
  Two machines that record the same decision (identical `kind`, `title`, and
  `body`) derive the same identity from that content, but `spelunk memory list`
  and `spelunk context` read every copy back and showed each one, so a decision
  both teammates had recorded looked like two competing entries. Those reads now
  fold copies by identity: one entry, `tags` and `linked_files` unioned across
  the copies (added, never removed), and the earliest recording time kept. An
  entry archived on any machine reads as archived everywhere. This matches the
  identity-keyed dedup `memory reconcile` and `spelunk init` already do on
  import.

  Reads that walk every note are also substantially faster. The fold has to see
  every copy of an entry before it can emit one, so a read can no longer stop as
  soon as it has enough entries the way it did before; note blobs are therefore
  read with a single `git cat-file --batch` rather than one `git notes show`
  subprocess per note.

- **`spelunk init` no longer breaks plain `git fetch` / `git pull`, and no
  longer lets a fetch destroy your unpushed memory.** The fetch refspec `init`
  configured, `+refs/notes/spelunk:refs/notes/spelunk`, had two failures. It is
  non-glob, so it required the remote ref to exist: in any repository where
  nobody had pushed notes yet, `git fetch origin` exited 128 and `git pull`
  exited 1 with `fatal: couldn't find remote ref refs/notes/spelunk`. And its
  leading `+` force-updated your working notes ref, so a plain `git fetch`
  silently replaced a local note you had not pushed with the remote's, reported
  only as `(forced update)` and recoverable only via the reflog. The refspec is
  now `+refs/notes/spelunk*:refs/notes/origin/spelunk*`, which fetches into a
  *tracking* ref: the glob tolerates a missing remote ref, and the tracking
  destination leaves your own notes alone. Fetched notes therefore arrive on
  `refs/notes/origin/spelunk`, and spelunk merges them into `refs/notes/spelunk`
  itself (see below), so travel is now fetch + merge.

  **If you ran `spelunk init` on a build from `main` between 2026-07-12 and this
  change, remove the old refspec by hand.** No release shipped it (v0.9.3
  predates it), so there is no automatic migration, and re-running `init` does
  *not* fix it: the idempotence check matches only the exact string, so you would
  keep the clobbering refspec and gain the new one alongside it. Run:

  ```bash
  git config --unset --fixed-value remote.origin.fetch '+refs/notes/spelunk:refs/notes/spelunk'
  ```

  `--fixed-value` needs git >= 2.30. On older git, escape the leading `+`, which
  git otherwise reads as an invalid regex (`error: invalid pattern`):
  `git config --unset remote.origin.fetch '\+refs/notes/spelunk:refs/notes/spelunk'`.
  Then re-run `spelunk init` to add the corrected refspec. (ADR-069)
- **Teammates' fetched memory is now visible without any extra step.** Because
  notes now land on a tracking ref, nothing merged them into your own, so a
  teammate's entry would have stayed invisible to `memory list`. `spelunk memory
  list`, `spelunk context`, and `spelunk init` now merge
  `refs/notes/origin/spelunk` into `refs/notes/spelunk` with git's
  `cat_sort_uniq` strategy: a union, so no conflicts, no duplicates, and neither
  side's entries are dropped. The merge is local-only and performs **no**
  network, so reads work with the remote unreachable; it merges only what your
  own `git fetch` already brought down. It is serialized by the notes lock, and
  if the lock is busy the merge is skipped and the read proceeds anyway (the
  union is idempotent, so the next read catches up). Your `notes.mergeStrategy`
  is never written; the strategy is passed per-invocation. *Publishing* your own
  memory stays opt-in: install the pre-push hook (see Added) or push
  `refs/notes/spelunk` by hand. (ADR-069)
- **Memory entries now read back in chronological order after a merge.** The
  union merge sorts lines lexicographically, so a note's records are no longer
  in append order once teammates' entries are folded in. Reads now sort by
  `created_at` explicitly rather than relying on blob order. (ADR-069)
- **Memory attached to a commit now survives `git commit --amend` and `git
  rebase`.** git carries a note onto a rewritten commit only when
  `notes.rewriteRef` names the ref, and it has no built-in default, so an
  unconfigured repository silently orphaned every entry the rewrite touched: the
  note stayed bound to the dead sha, and `memory list` never surfaced it again.
  Pre-`init`, git notes is the sole store, so that was total loss of the only
  copy. spelunk now points `notes.rewriteRef` at `refs/notes/spelunk` in the
  repository's own config (never global) at `spelunk init`, at the first
  pre-`init` note write, and on the `--backend git-notes` write path. It composes
  with any value you set yourself rather than replacing it, honours an existing
  value that already covers the ref, announces itself on the run that sets it,
  and warns without failing when it cannot be written. `notes.rewriteMode` is
  deliberately left at its `concatenate` default, which keeps both entries when
  two noted commits are squashed together. Known gap: git honours the setting for
  `amend` and `rebase` only, so `git merge --squash` and cherry-picking onto a
  divergent base still do not carry notes. See
  [docs/memory.md](#surviving-history-rewrites). (ADR-068)
- **The Debian package no longer installs a `spelunk` that cannot start.** The
  `.deb` declared `libc6` as its only dependency, omitting `libdbus-1-3`, which
  the `spelunk` binary dynamically links via the keyring's secret-service
  backend. On a machine without libdbus already present the install *reported
  success* and the binary then failed to load with `libdbus-1.so.3: cannot open
  shared object file` (exit 127). `Depends:` is now derived from the packaged
  binaries at release time with `dpkg-shlibdeps` instead of being hand-written,
  so it lists every shared library they actually link (`libc6`, `libdbus-1-3`,
  and `libgcc-s1`) and the package manager pulls them in. The declared `libc6`
  floor now also reflects the glibc the release binaries are built against
  (2.39), so a system with an older glibc gets an accurate refusal at install
  time rather than a package that installs and then crashes. A release-pipeline
  check now installs the `.deb` in a clean container and runs both binaries, so
  a missing dependency fails the build instead of shipping.
- **The self-hosted Docker Quick-Start builds and runs again.** The `Dockerfile`
  now builds the Cargo workspace (per-crate manifests, `crates/spelunk-server`
  binary) instead of the old single-crate layout, and installs the C/C++
  toolchain plus libdbus the slim base lacks so the tokenizers build script and
  the keyring backend link. `docker-compose.yml` no longer aborts a bare `docker
  compose up` when `SPELUNK_SERVER_KEY` is unset (the profiled team-server key is
  no longer evaluated at parse time), so the default loopback scaffold comes up
  with no key while the team-server profile still requires the key and TLS. Both
  services set `pull_policy: build`, so the image is built locally with no
  registry pull (air-gapped friendly). A CI job now builds and smoke-runs the
  image so a broken build is caught on PRs.
- **`spelunk context` no longer lists duplicate convention records per
  language.** Convention rows are now merged to one per (language, category)
  before storage, so overlapping generic and language-specific rules (for
  example `naming.functions` and `docs`) each surface once instead of two or
  three times.
- **`spelunk context` no longer lists `.tsx` conventions twice, and no longer
  overstates their confidence.** Conventions from `.tsx` files surfaced under
  both a `typescript` and a `tsx` label, because the TypeScript rule set labelled
  every record `typescript` while the generic rules labelled by the chunk's own
  language. `.tsx` now folds onto the `typescript` label it already shares
  heuristics with, so each convention appears once. The two labels had also held
  separate partial views of a single language, and merging them kept the *higher*
  of the two confidences: a project with 9 async functions out of 16 reported
  `async` at 100% instead of 56%. Confidence is now pooled across all of a
  language's chunks, so mixed `.ts`/`.tsx` projects may see reported confidence
  drop. The lower figure is the accurate one, and no conventions are lost.
- **`spelunk server stop` reliably terminates a wedged local server.** A daemon
  whose `/v1/health` had stopped responding could not be stopped and was
  silently orphaned across a `stop && start`. `stop` now recognises a hung
  daemon as ours, sends SIGTERM, escalates to SIGKILL after a bounded wait, and
  reports success only once the process is confirmed gone. `start` reclaims a
  stale or hung prior daemon on the requested port instead of drifting to a
  different port (which left two servers on one `server.db`), and fails loudly
  if an unrelated process holds the port. A single-instance guard prevents two
  servers running against the same `server.db`.
- **`spelunk index` no longer silently drops LLM summaries.** The summary pass
  ran detached from the rest of the run, so an index whose other phases finished
  first could exit with summaries still in flight: they were never generated,
  and nothing reported it. `index` now finishes summaries before it returns.
  Generation stays best-effort (a failure warns and never fails the run, so git
  hooks still exit 0), and `--detach` / `--detach-embed` still background the
  whole run.
- **`spelunk index` no longer reports success for summaries the LLM never
  produced.** Against an unreachable or failing LLM the run printed `Summarised
  1 batch(es).` and exited 0 while storing empty summaries. Failed batches are
  now excluded from that count (`Summarised 0 batch(es).`) and a warning reports
  how many produced nothing; `RUST_LOG=warn` shows the cause. Retrying needs
  `spelunk index --force`, since a chunk whose summary failed is recorded as
  attempted and a plain re-run skips it.
- **Background-phase diagnostics are no longer discarded.** Errors and warnings
  from the detached `--_background-phases` child (on repos over 100 files) and
  `--_embed-phases` child (with `--detach-embed`), including LLM summary
  failures and remedies, now route to `.spelunk/index-background.log` with a
  user-visible pointer on the status line. The log is bounded (truncated per
  run).

  **Upgrade note:** existing projects must add `*.log` to `.spelunk/.gitignore`
  by hand, since the template is written only on first init. Run:
  ```bash
  echo "*.log" >> .spelunk/.gitignore
  ```
- **`spelunk org switch` now stays in effect after the access token expires.**
  The switched-to org lived only in the short-lived (~5 minute) WorkOS access
  token; nothing re-applied it when that token was refreshed. The first token
  refresh after any `org switch` — triggered by the next `spelunk memory push`,
  `sync`, or other cloud-api/team-server call — silently reverted the session
  to the account's default org and persisted the reverted token, with no error
  or warning. Anything pushed after that point landed in the wrong org.
  Refreshes now re-send the durably stored active org, so a switched org
  survives rotation and stays in effect until you switch again.
- **macOS no longer prompts for keychain authorization multiple times per
  `spelunk` invocation.** `Config::load` read the personal-store `server_key`
  unconditionally, even when an environment variable or a WorkOS `[auth]`
  token already outranked it and made the read pointless, and the CLI's
  pre-parse `--help` gate ran a full `Config::load` of its own ahead of the
  real one, so a single command could touch the keychain several times with
  nothing cached in between (each uncached read is a separate OS
  authorization on macOS; per-item ACLs don't dedupe across accesses). Three
  changes close this: the pre-parse gate now checks only whether `llm_model`
  is configured, read straight from the config file, without constructing a
  secret store at all; `Config::load`'s `server_key` resolution skips the
  personal-store read entirely once an env var or `[auth]` token already
  resolves the bearer; and the keychain-backed store now caches each key's
  value process-wide, so a key that is read is fetched from the OS keychain
  at most once per invocation no matter how many call sites ask for it.
- **`spelunk index` on repos over 100 files could summarize against the wrong
  config, or silently ignore `--no-summaries`/`--summary-batch-size`, once
  indexing continued in its detached background phase.** Above the 100-file
  threshold, indexing hands graph rank, spec discovery, and LLM summaries to a
  detached child process; that child (and the separate one spawned by
  `--detach-embed`) rebuilt its own command line from scratch instead of
  forwarding the parent's, so it re-resolved the default config in place of
  whatever `--config` the parent had resolved (summarizing against the wrong
  config, or skipping the pass entirely if that default config has no chat
  model configured), and dropped `--summary-batch-size` from both spawns and
  `--no-summaries` from the background-phases spawn (so a run given
  `--no-summaries` could still generate them in the background). All three are
  now forwarded through one shared argv-building function used by both spawn
  sites. `spelunk index` still exits 0 if summarization fails in the child;
  this only changes what the child is given, not what it does with a failure.

### Known issues

- **`memory archive` and `supersede` do not yet travel via the git-notes carrier.**
  Archiving or superseding an entry updates your local `memory.db`, but that state
  change is not yet propagated through `refs/notes/spelunk` to teammates: entries
  sync, their archived/superseded state does not. A fix is tracked for a follow-up
  release.

## [0.9.3] — 2026-07-08

### Removed

- **`spelunk search --as-of <sha>` (snapshot-based temporal search).** The snapshot
  storage layer was never wired to the indexer — `list_snapshots()` was always empty
  and the flag errored on every use. Temporal/as-of semantic search is deferred to a
  future design. The dead snapshot tables (`snapshots`, `snapshot_files`,
  `snapshot_chunks`, `snapshot_embeddings`) are dropped via migration 021 on any
  database opened with this version; existing `spelunk search` (without `--as-of`)
  continues to work unchanged. Note: `spelunk memory list/search --as-of <date>` for
  point-in-time memory archaeology remains available and unaffected. (#517)
- **Retired the `api_base_url` client egress path and pruned the remaining dead
  config keys** (`plans_dir`, `specs_dir`, `batch_size`, …) from the config surface
  and the shipped `examples/mdm/spelunk-config.toml`. Existing config files that
  still carry these keys continue to parse unchanged (forward-compat locked by a
  regression test). (#532, #551)

### Security

- **`spelunk-server` now refuses a keyed non-loopback plaintext bind, with no
  override.** Previously the startup guard only refused a *keyless* non-loopback
  bind (an open, unauthenticated server). A *keyed* non-loopback bind over
  plaintext HTTP was still allowed, so a shared server on a routable interface
  sent the bearer `SPELUNK_SERVER_KEY` across the network in cleartext. The guard
  now refuses a non-loopback plaintext bind unconditionally, whether or not a key
  is set; the error names the interface/port. There is no opt-out. Loopback
  binds are unchanged. See
  `docs/server.md#non-loopback-plaintext-binds-are-refused-no-override`.
  - **Docker Compose demoted to a local scaffold; bare-metal/systemd is now
    the recommended team-server deployment.** The shipped `docker-compose.yml`
    previously bound the `spelunk-server` container to `0.0.0.0` directly,
    which now refuses to start as soon as `SPELUNK_SERVER_KEY` is set —
    exactly the documented keyed quick-start. Rather than ship a proxy sidecar
    to work around that, `docker-compose.yml` is stripped to just
    `spelunk-server` (binding loopback inside its own container, the
    Dockerfile's default) and a named volume for the SQLite database, with no
    published port: a container's loopback lives in its own network
    namespace, unreachable from the host or a sibling container by any of the
    usual means (bridge port-publish, Docker Desktop host-mode, or
    container-to-container DNS), so Docker is a poor fit for networked/team
    serving. It remains useful as a minimal local scaffold for running the
    server process itself. For a team-reachable instance, run the binary
    bare-metal under systemd instead, with your own TLS terminator in front of
    the same loopback bind on that host — see
    `docs/self-hosting.md`. `docker-compose.full.yml` (Ollama
    sidecar) and `Caddyfile` (bundled TLS sidecar) are removed; no proxy ships
    with this repo. See `docs/server.md#quick-start-docker`.
- **Server robustness/info-leak hardening (error-string sniffing, raw FTS5 errors, unbounded
  file reads).**
  - `AppError::Internal` no longer inspects the error message text (previously it returned the
    raw error string to the client whenever it contained `"mismatch"` or `"required"`). The one
    legitimately-safe case — a project's configured embedding dimension not matching an
    incoming vector — is now a typed `DimensionMismatch` error mapped to a 400 with a fixed
    safe message; everything else funnels through `AppError::Internal` to a fixed generic
    `"Internal server error"` 500, regardless of what the underlying error's `Display` text
    says. Closes the V1-SERVER-AUDIT §8 "5xx do not leak stack traces or internal paths" gap.
  - Full-text search queries (`spelunk search --mode text` and the server's `/v1/search`
    hybrid search) are now quoted as FTS5 string literals before being bound to `MATCH`
    (internal `"` doubled per FTS5 escaping rules), so punctuation in a search term — `"`,
    `:`, `OR`/`NOT`/`NEAR`, unbalanced parens, etc. — no longer surfaces a raw FTS5 query-parse
    error to the caller. **Known gap:** a query term containing an embedded NUL byte still
    leaks a raw FTS5 parse error despite the quoting (FTS5's own parser appears to treat `\0`
    as an early string terminator, independent of SQLite's NUL-safe `TEXT` binding); tracked as
    a follow-up.
  - `spelunk index` now enforces a `MAX_FILE_BYTES` (64 MiB) cap via `metadata().len()` before
    reading a file into memory, applied uniformly across every format (text/Markdown/tree-sitter
    source, PDF, DOCX, XLSX) — previously the cap only bounded an already-read buffer on the
    tree-sitter branch, so a very large or maliciously crafted file of any other supported
    format could still be read fully into memory first. Oversized files are now skipped with a
    warning instead. This is local-indexing hardening, distinct from and complementary to the
    server-side request-body caps shipped in 0.9.2 above.
- **`CloudSyncClient` refuses to attach a bearer token to a non-HTTPS `server_url`
  at construction.** A team `server_url` set to plaintext `http://` no longer sends
  the bearer `SPELUNK_SERVER_KEY` in cleartext — the client fails fast at
  construction, closing the CLI-side gap adjacent to the server bind-safety work
  above. (#549)
- **`add_note` now audit-logs notes rejected by injection-pattern detection**, so a
  rejected write leaves a trail instead of failing silently.
- **Bumped `crossbeam-epoch` to 0.9.20** for RUSTSEC-2026-0204. (#535)

### Added

- **`bench/paired_stats.py` for publishing agentic benchmarks:** McNemar's exact test with paired task outcomes, bootstrap 95% CIs over per-seed means, deterministic n=1 handling, and cell-labeled output refusing to aggregate across differing model/harness/condition. Committed example fixtures in `bench/results/examples/`. Run: `python bench/paired_stats.py <baseline.json> <condition.json>`.
- **Benchmark `vanilla_rag` condition: plain embed-and-KNN control.**
  `bench/memory/decision_archaeology.py` now includes a fifth baseline condition
  that embeds raw commit messages with the native F2LLM embedder and ranks by
  cosine similarity, isolating the lift of `memory_search` from harvesting and LLM
  extraction. 20 offline unit tests in `bench/memory/tests/test_vanilla_rag.py`.
- **`--harness none|opencode|claude-code` matrix for the SWE-bench benchmark scripts** (`bench/agents/`) — the same (task, model, condition) cell can now be run under three different coding agents so `condition` (spelunk tool access) and `harness` (which agent framework drives the loop) vary independently:
  - `bench/agents/harness_opencode.py` and `bench/agents/harness_claude_code.py` are new single-task runners, siblings of the existing `agent.py` (now the `harness=none` runner — this repo's own OpenAI-compatible tool-calling loop, no external framework in the loop at all). `harness_opencode.py` shells out to headless `opencode run` (DeepSeek wired in via opencode's native custom-provider mechanism, not a compat shim); `harness_claude_code.py` shells out to headless `claude -p`, reaching DeepSeek via its documented Anthropic-compatible endpoint (with a `--endpoint-kind shim` fallback for an Anthropic→OpenAI proxy if that endpoint misbehaves).
  - `bench/agents/harness_common.py` centralises patch extraction (`extract_patch`) shared by both new adapters, so the diffing/staging logic never varies across harnesses. This also fixes a latent bug: `git add -- <pathspecs>` fails fatally (and stages nothing at all) when *any* pathspec matches zero files, which fires on essentially every real SWE-bench task repo — the extraction now first asks `git diff --name-only` / `git ls-files --others` (both pathspec-tolerant) for the concrete changed-file list before staging, so a real fix no longer silently produces an empty patch.
  - `swebench_run.sh` gained `--harness` and `--endpoint-kind`/`--no-deepseek` passthrough, and writes harness-suffixed results/patches paths (`swebench-<condition>-<harness>-<timestamp>`) for the two new harnesses while keeping the original unsuffixed filename convention for `--harness none` so existing tooling that globs for it keeps working.
  - The provenance/reproducibility contract (`bench/agents/README.md`) gained `harness`, `harness_version`, `endpoint_kind`, `effort`, `thinking`, `run_seed`, plus reserved `question_set_version`/`instance_filter`/`judge_*` fields — additive only, so every harness's result JSON (including `harness=none`) is a strict key-superset of the pre-existing schema and old consumers reading specific keys via `.get()` are unaffected.
- **Benchmark test suite** (`bench/agents/tests/`) — comprehensive offline pytest coverage for the SWE-bench harness matrix, including patch extraction, opencode provider config, swebench_run.sh argument validation, and provenance contract verification across all three harnesses (`none`/`opencode`/`claude-code`). Run with `uv run --with pytest pytest bench/agents/tests/ -v`. No API keys, network, or external harness binaries required.

### Features

- **`spelunk index --detach-embed`: background embedding on slow hardware.**
  When embedding a large codebase on slow hardware, parsing can now run in the foreground (so
  text/ast-grep search is immediately available) while the long embedding phase runs in the
  background. Useful for CI/CD and multi-corebot indexing workflows where waiting for full
  embeddings blocks other tasks unnecessarily. Run `spelunk status` to check progress (shows
  "Embedding in progress: N/M embedded" when a background or interrupted embed is underway). If
  the background pass is interrupted (machine sleep, process killed, network downtime), simply
  re-run `spelunk index` to resume from where it left off.
- **Embedding progress bar displayed immediately during indexing.**
  The ETA-aware embedding progress bar now appears as soon as the embed phase begins, instead of
  waiting for the first batch to complete.
- **`spelunk-server --health-check`: a self-contained container health probe.**
  It probes the server's own `/v1/health` on the configured `--host`/`--port` and exits `0`
  (live) or non-zero, so a container `HEALTHCHECK` needs no `curl`/`wget` in the runtime image.
- **First-party systemd units + credential-based API keys for the team server.** `spelunk-server`
  can now read its shared API key from `--key-file` or a systemd `LoadCredential` credential — a
  first-class alternative to `SPELUNK_SERVER_KEY` that keeps the key out of the process table —
  and the repo ships packaged systemd units for running the bare-metal team server.

### Fixes

- **`spelunk-server` bounds candle's CPU embedding threads so embeds no longer starve request
  serving.** On CPU-only hosts a single embed batch previously fanned candle's
  gemm across every core, pinning the machine and briefly hanging `/v1/health`. The server now
  caps the embedder's CPU thread budget to `max(1, cores − 2)` by default (override with
  `SPELUNK_EMBED_THREADS`, or a pre-set `RAYON_NUM_THREADS`), leaving headroom to keep serving
  requests during indexing.
- **The chunker caps oversized semantic and Markdown chunks** so a single very large unit no
  longer stalls the embed phase.
- **`spelunk init` writes `.spelunk/.gitignore`** so machine-specific SQLite files aren't
  accidentally committed.
- **The CLI no longer surfaces a phantom `plan` capability** it never implemented. (#540)
- **`spelunk index` no longer loses computed embeddings when a batch times out on slow hardware.**
  The embed phase now calibrates against real timing before committing to a
  batch size: it sends a 1-chunk request, then a 4-chunk request, and derives the per-request
  batch size (and its timeout) from the measured per-chunk rate — a small batch on slow hardware,
  a large one (up to the 256-chunk server limit) on fast hardware — re-estimating as later
  batches land so a mid-run rate change is picked up rather than locked to the first sample. Each
  batch is persisted to the database before the next request, so an interrupted run (due to
  timeout, machine sleep, or process termination) can resume by re-running `spelunk index`. Prior
  batches are retained, and already-embedded chunks are skipped.
- **`spelunk index` embedding could fail immediately with a server `408 Request Timeout`, even
  on the very first (single-chunk) request.**
  The calibration design above targets a ~240s round trip per batch (scaling down on slower
  hardware), but `spelunk-server`'s general request-handling middleware enforced a blanket 30s
  budget on every route — including `/index/embed` — so any batch sized for the calibration
  target, or even a single oversized/slow-to-embed chunk on CPU-only hardware, could be killed by
  the server before the CLI's own (much longer) client-side timeout ever applied. `/index/embed`
  now has its own, much larger request budget (1800s, matching the CLI's own timeout ceiling),
  while every other route keeps the original 30s budget — the same "long-lived, exempted from
  the general timeout" pattern already used for `/memory/stream`'s SSE connections. `/v1/health`
  now also advertises the server's operative `/index/embed` limits (request budget, max batch
  size, embedder token cap), and the CLI reads them to size its calibration to the server it's
  actually talking to — including a conservative fallback (small batches, with a one-line notice)
  when talking to an older server that predates this fix and still enforces the old 30s budget.
  Two related calibration bugs surfaced by this fix are also corrected: the very first
  (single-chunk) calibration sample no longer gets equal weight against the second sample when
  estimating throughput (it's dominated by one-off connection/cold-start overhead and was
  skewing the estimate the batch-size decision relies on), and the batch size can no longer grow
  by an arbitrarily large multiple in a single step (capped to 8x the previous batch). A
  calibration request that still times out is now retried once with escalated patience before
  giving up, and a steady-state batch that hits the server's budget shrinks and retries instead
  of aborting the whole run at whatever had been embedded so far.

## [0.9.2] — 2026-07-03

### Security

- **CLI local-hardening bundle** (`server stop`, state file perms, `registry
  autoclean`, web-to-md hook path):
  - `spelunk server stop` now verifies the recorded server is actually alive
    and healthy (via `/v1/health`) before signaling its PID, instead of
    signaling on a bare liveness check. Prevents killing an unrelated process
    that has since reused the old PID after a crash.
  - The state directory and `server.pid`/`server.port`/`server.log` files are
    now created `0700`/`0600` (previously default/world-readable perms), and
    writes no longer follow a pre-planted symlink at those paths.
  - `registry autoclean` now refuses to `remove_dir_all` a project's
    `.spelunk` directory if that path is a symlink, instead of deleting
    through it.
  - **Breaking:** the optional `memory add --from-url` web-to-Markdown hook
    script must now live at `~/.config/spelunk/scripts/web-to-md.ts` — the
    previous `~/scripts/web-to-md.ts` location is no longer read. See
    [docs/memory.md](docs/memory.md#web-to-md-hook) for migration. This closes
    an implicit-code-execution path where any script an attacker could plant
    under `$HOME/scripts/` (e.g. via a prior unrelated compromise, or on a
    shared/managed machine) would run automatically.
- **Breaking: non-loopback `http://` URLs are now rejected as invalid config.**
  `server_url` and any configured inference URL must be `https://` unless they
  point at loopback (`127.0.0.1`, `::1`, `localhost`) — the CLI attaches your
  `Authorization: Bearer` token to these requests, so a plaintext non-loopback
  URL previously sent it in the clear. There is no opt-out. **If your
  `.spelunk/config.toml` has `server_url = "http://<host>:<port>"` pointing at
  anything other than loopback, spelunk will now refuse to start** with a
  one-line error telling you to switch to `https://` (put a TLS-terminating
  reverse proxy in front: see `docs/self-hosting.md`) or move
  the server to loopback. Loopback `http://` and all `https://` URLs are
  unaffected.
- **The CLI no longer sends the bearer token to `/v1/health`.** That endpoint
  is unauthenticated on the server side (see below), so the probe no longer
  attaches `Authorization` to it — matching the server, which never required
  it. Authenticated endpoints (search, memory, inference) are unaffected.
- **Hardened `spelunk-server`'s DoS surface: timeouts, body/concurrency caps, input-length
  limits, and `/explore` rate limiting.** `router()` now attaches a `tower_http` middleware
  stack — a 30s `TimeoutLayer` on every route except `/memory/stream` (exempt as a deliberate
  long-lived SSE connection), a 2 MiB `RequestBodyLimitLayer` on every route, and a global
  `ConcurrencyLimitLayer` (256 concurrent requests). Memory writes now enforce input caps at the
  handler (title ≤ 500 chars, body ≤ 50,000 chars, embedding vector length must match the
  server's configured dim; project-id slugs capped at 200 bytes), returning 400 on violation.
  `/explore` is now rate-limited the same way `/llm/complete` already was, closing an unmetered
  token-burn hole (up to `2048 * max_turns` generated tokens per call were previously free); both
  endpoints now key their rate-limit bucket on **principal + client IP** rather than principal
  alone, so a shared team key no longer collapses every distinct caller onto one bucket. Fixed a
  related bug along the way: `/explore` and `/llm/complete` hand generation to a detached
  `tokio::spawn` after constructing their streaming response, so the outer `TimeoutLayer` never
  actually bounded a hung LLM backend on those two routes; the spawned `generate()` call is now
  separately wrapped in its own `tokio::time::timeout`. See `docs/security/THREAT-MODEL.md`
  ("D — Denial of Service") for the full threat breakdown, including a known limitation where
  `ConcurrencyLimitLayer`'s permit release doesn't yet bound concurrent *streaming* sessions on
  `/explore`/`/llm/complete`.
- **Suppressed two `quick-xml` DoS advisories with no upstream fix
  (RUSTSEC-2026-0194, RUSTSEC-2026-0195).** `quick-xml` is pulled transitively by
  the `calamine` (XLSX/ODS) and `docx-rs` (DOCX) document parsers used during
  `spelunk index`; both crates are on their latest release and still pin
  `quick-xml < 0.41`, where the fixes land, so there is no version to upgrade to.
  The exposure is a local denial-of-service while indexing a maliciously crafted
  office document (no memory unsafety, no data exposure). The advisories are
  ignored in `.cargo/audit.toml` and `deny.toml` with a re-check note, to be
  dropped once the parsers bump `quick-xml`. The vestigial repo-root `audit.toml`
  (never read by `cargo audit`, which loads `.cargo/audit.toml`) was removed.
- **Insecure temp file for the `spelunk memory add`/edit `$EDITOR` draft.** The
  draft body is now created with `tempfile::Builder` (unpredictable name,
  `O_EXCL`, mode `0600` on unix) instead of a PID-derived path in
  `std::env::temp_dir()`, closing a local symlink/TOCTOU clobber and a
  world-readable info-leak window. The read-back after the editor exits now
  goes through the retained file handle instead of re-opening by path, so a
  symlink swapped in during the edit window can't be followed.
- **Fixed argument injection in `spelunk memory harvest`'s `git log` invocations.**
  `--branch`/`--git-range` values were passed straight to `git log` without a
  `--` separator, so a value starting with `-` (e.g. `--output=/path/to/victim`)
  could be parsed as a git option instead of a revision, enabling arbitrary-file
  overwrite. Both `git log` call sites now add the `--` separator and reject any
  ref/range endpoint that starts with `-` before it reaches git. As
  defense-in-depth, the git-notes write path now passes note bodies via stdin
  (`-F -`) instead of `-m <arg>`, so they can never be parsed as options and no
  longer appear on argv/process listings.
- **Constant-time API key comparison in `spelunk-server`.** Bearer-token auth
  compared the provided token against the configured key with a plain `&str ==`,
  which short-circuits on length and first differing byte — a timing side
  channel on a network-exposed team server. The configured key is now hashed
  once with BLAKE3 at startup and compared against a per-request hash of the
  provided token using `constant_time_eq`; loopback (no-key) behaviour is
  unchanged.
- **`spelunk-server` refuses to bind a non-loopback address without an API key
  configured.** Previously it would happily expose an unauthenticated endpoint
  off-host. `--host`/`SPELUNK_SERVER_KEY` are checked at startup, before any DB
  or embedder work: a non-loopback bind (`0.0.0.0`, `::`, a LAN/public IP) with
  no key now fails to start. Loopback binds are unaffected. **Breaking for the
  keyless Docker quickstart**: the container image binds `0.0.0.0` by default,
  so `docker compose up -d` with no `SPELUNK_SERVER_KEY` set now refuses to
  start: see `docs/server.md#quick-start-docker`.
- **ADR-056 single-trust-domain guardrails.** Per [ADR-056](docs/adr/056-oss-server-tenancy-model.md),
  a `spelunk-server` instance's shared API key is the tenancy boundary by
  design — every keyholder administers every project on that instance; there is
  no per-project ACL. The server now logs a prominent startup warning restating
  this whenever it binds a non-loopback address with a key configured (a
  shared/team deployment); suppressed on loopback binds and when no key is
  configured. See `docs/server.md#trust-model`.
- **Secret scanner now scans the docstring and LLM summary, not just the raw
  chunk content.** `Chunk::embedding_text()` prepends the docstring (and, once
  generated, the LLM summary) to what actually gets stored and embedded, but the
  scanner previously only checked `chunk.content` — a credential living only in
  a doc-comment or an LLM summary was persisted and embedded unscanned. Also
  added detection patterns for GCP API keys, SendGrid keys, Twilio key SIDs, npm
  `_authToken` assignments, and Azure storage/service-bus connection strings, and
  made sensitive-file exclusion globs (`*.pem`, `id_rsa`, etc.) case-insensitive.
  Still best-effort defense-in-depth, not a security boundary — see
  [docs/architecture.md](docs/architecture.md).

### Fixed

- **First-run auto-start no longer misreports failure while the model loads.**
  `spelunk-server` now binds its listener and serves `/v1/health` **before**
  loading the native embedder (the ~339 MB model now loads on a background task),
  so the CLI's auto-start health check sees a live server immediately instead of
  timing out during the first-run download. The auto-start/`spelunk server start`
  wait was also extended to 30 s, and its failure warning now fires only on a
  genuine liveness timeout (with firewall + `spelunk server logs` guidance).
- **`spelunk index --batch-size` was a silent no-op.** The flag was parsed but
  never passed to the embed phase, which always used a hardcoded 64-chunk batch.
  It's now threaded through and clamped to the server's 256-chunk ceiling
  (falls back to the previous 64 default when unset).

### Added

- **Full semantic indexing — AST chunking plus import/call/inheritance graph
  edges — for PHP, Ruby, C#, Kotlin, and Swift.** These five languages
  previously only got plain structural (`ast-grep`) search; `spelunk index` now
  extracts the same chunk/edge fidelity already available for Rust, Python, Go,
  Java, and the rest. `spelunk languages` / `SUPPORTED_LANGUAGES` grew
  accordingly; see the [Supported languages](README.md#supported-languages) list.
- **`spelunk login` resolves your org automatically after a no-org device
  login**, instead of leaving a session that needs a follow-up `spelunk org
  switch`. WorkOS doesn't auto-select an org even for single-org accounts: a
  plain `spelunk login` (no `--org`) now silently selects your org when you
  have exactly one, offers an interactive selector when you have several and
  you're on a TTY, and otherwise errors with an actionable "pass `--org`"
  message (no hang on a non-interactive/agent shell). Zero orgs gets a clear
  onboarding message with no dangling session persisted. The scripted `--org
  <slug>` path is unchanged.
- **`spelunk sync --project <slug>`** selects the cloud project to sync into.
  Required on first sync when no `project_id` is configured — `spelunk sync`
  never auto-derives a project name from the folder or git remote; it now halts
  with an actionable message pointing at `--project` instead. Repeat syncs with
  the same slug reuse the (lazily server-created) project.

### Changed

- **Tree-sitter grammars now come from `ast-grep-language`**, a single crate
  replacing 13 hand-pinned `tree-sitter-*` deps (`proto` and `sql` stay on
  standalone grammar crates, which `ast-grep-language` doesn't ship). Internal
  dependency consolidation; chunking and graph-edge output are unchanged for
  existing languages. (#487)
- **`spelunk-server` now binds `127.0.0.1` by default** (was `0.0.0.0`). Loopback
  is the safer, firewall-exempt default; pass `--host 0.0.0.0` explicitly to
  expose the server on all interfaces. The container image sets `--host 0.0.0.0`
  in its entrypoint so published ports stay reachable. The CLI auto-spawned daemon
  already forced `--host 127.0.0.1` and is unaffected.
- **`/v1/health` now reports embedder readiness** via a new
  `embedder: { state, detail }` sub-object (`loading` | `ready` | `unavailable` |
  `disabled`). `capabilities` (`index.embed` / `search.semantic`) and
  `embedding_dim` are populated only once the embedder is `ready`
  (backward-compatible). While the embedder is warming up, the embed/search
  endpoints return `503` (with `Retry-After: 5` while `loading`, terminal while
  `unavailable`) instead of the `400` now reserved for the genuinely unconfigured
  case.

## [0.9.1] — 2026-06-30

### Fixed

- **Single-chunk embedder OOM on large files.** The native embedder now caps a
  single chunk's attention by a RAM-aware memory budget; chunks that would exceed
  the budget are truncated (or skipped) instead of allocating an attention tensor
  large enough to exhaust memory and crash indexing of large files. (#475)
- **`spelunk org switch`** now persists the selected `workos_org_id`, refreshes the
  access token against the correct organization, and uses the production WorkOS
  client id. (#478)
- Corrected stale and inaccurate `--help` text across all commands. (#477)

### Added

- **Official Windows build.** Releases now publish an `x86_64-pc-windows-msvc`
  `.zip` artifact and an `install.ps1` one-liner installer, with Windows added to
  the CI build and test matrix. (#453, #444)
- OS-keychain credential storage for the CLI bearer credential, with automatic
  migration of any existing plaintext-config credential into the keychain.
  `SPELUNK_SECRET_STORE=file` forces the previous file-based store for headless
  environments. (#456)
- Organization name is now shown in the `spelunk login` confirmation. (#452)
- `spelunk-server --version` flag; `--help` output is now prefixed with the
  version line. (#449)
- Pre-flight embedding-dimension check in the CLI probe, guarding against a stale
  loopback server with a mismatched embedding dimension. (#457)
- Enterprise MDM deployment example for `spelunk` and `spelunk-server`. (#472)

### Changed

- The native embedder now downloads a **pre-quantized Q8_0 GGUF by default**
  (~339 MB) instead of downloading the ~638 MB BF16 weights and quantizing on
  device. Set `SPELUNK_EMBEDDER_GGUF_REPO=off` to build from the upstream weights
  on device, or point it at another repo to override. Cached BF16 safetensors are
  removed after the GGUF is written; the upstream embedder revision is pinned.
  (#474, #479)

## [0.9.0] — 2026-06-26

### Breaking changes — migration required

**Default embedder is now F2LLM-v2-330M via candle, 896-dim, GPU-accelerated on macOS.**

The bundled native embedder has switched from fastembed-rs / Nomic Embed Text v1.5
(768-dim, ONNX) to **codefuse-ai/F2LLM-v2-330M** (896-dim, Qwen3 decoder) served via
the `candle` runtime. On macOS the prebuilt binary uses Metal GPU acceleration; Linux
falls back to CPU. The model auto-downloads from Hugging Face Hub on first run and
caches locally; there is no external embedding service to run. (#439)

The weights are quantized to **Q8_0** and cached as a GGUF (`f2llm-v2-330m-q8_0.gguf`)
in `~/.local/share/spelunk/models/`: the ~650 MB BF16 safetensors are downloaded once,
quantized, and written to a ~339 MB GGUF, so subsequent loads read the GGUF directly
with no network access and no safetensors load (roughly half the on-disk footprint).
(#441)

**Re-index required.** Two changes make existing local indexes incompatible:

- The embedding dimension changed 768 → 896, so vectors from the old model will not
  produce correct results against F2LLM.
- Chunk and snapshot embeddings are now stored as sqlite-vec **`INT8[896]`** instead of
  `FLOAT[768]`, which makes the on-disk vector index roughly **4× smaller**. (Memory
  entry embeddings stay `FLOAT[896]`.)

On first open after upgrading, spelunk detects the old `FLOAT[768]` `vec0` tables and
drops and recreates them as `INT8[896]` automatically. Run `spelunk index <project>`
to re-embed. (#439, #441)

### Added

- **`spelunk login` / `spelunk org switch` / `spelunk logout`** — browser-based
  device login for spelunk.cloud with short-lived, auto-refreshing tokens.
  `spelunk login` prints a verification URL and a short user code; open the URL
  in a browser, enter the code, and approve the sign-in. Multi-org accounts
  select their organization on the browser-hosted approval page. On success,
  tokens are stored in the `[auth]` table of
  `~/.config/spelunk/config.toml` (mode `0600`). Polls for approval with
  back-off on `authorization_pending` / `slow_down`. The tokens are refreshed
  transparently on expiry, so you stay logged in without re-running `login`.
  `spelunk login --org <slug>` logs in and then re-scopes to the named org.
  `spelunk org switch <slug|uuid>` re-scopes an existing session without a new
  device login. `spelunk logout` clears the stored credentials. `--cloud-url`
  (or `SPELUNK_CLOUD_URL`) overrides the default `https://api.spelunk.cloud`
  endpoint. Existing setups using a static `server_key` / `SPELUNK_SERVER_KEY`
  keep working until the next `spelunk login`, and `SPELUNK_SERVER_KEY` still
  takes precedence for CI.

- **Two-way memory sync with spelunk.cloud (`spelunk sync` / `spelunk memory
  sync`).** When a team server is configured, `spelunk sync` (top-level alias
  for `spelunk memory sync`) now performs a real two-way sync: it pushes local
  memory entries the cloud has not seen and pulls remote entries into the local
  `memory.db`, applying changes keep-both so concurrent edits on multiple
  machines are preserved. A new `spelunk memory pull` does a one-way delta pull.
  Sync is identity-keyed on a time-ordered UUID carried by each entry, so it is
  idempotent (re-running never duplicates) and drift-free across machine clocks;
  archived entries propagate as tombstones. (#425)

- **Sync modes (`mode = offline | local_first | cloud_first`).** A new `mode`
  config field (and `SPELUNK_MODE` env override) controls how the CLI reconciles
  local and cloud memory. The default preserves existing behaviour: with no
  `server_url` the CLI is `offline`; with a `server_url` set it is `local_first`.
  `SPELUNK_NO_SERVER=1` remains a hard kill-switch. (#425)
- **Sync-mode indicator and state-scoped capability hints.** `spelunk status`
  gains a neutral one-word `mode` line reporting the active sync mode
  (`local_first`, `cloud_first`, or `offline`) whenever a `server_url` or an
  explicit `mode` is configured; it carries no call to action. Capability hints
  are now scoped to the configuration: the embedder hint points at the team
  server when an explicit `server_url` is configured (not the auto-discovered
  loopback); the explore hint truthfully names an unreachable configured server
  instead of suggesting to set one that is already set. `cloud_first` mode pins
  hard-error behavior: reads and writes fail loudly when the server is
  unreachable or untrusted, and local data is never silently substituted as a
  fallback.

### Changed

- **Cloud auth now uses short-lived, auto-refreshing tokens instead of a
  non-expiring key.** `spelunk login` stores access/refresh tokens under the
  `[auth]` table of the config; requests send the access token as a bearer
  credential and the CLI refreshes it transparently on expiry, retrying the
  original request once. Bearer precedence is `SPELUNK_SERVER_KEY` (env) > stored
  `[auth]` access token > legacy bare `server_key`, so existing `server_key`
  users keep working with no flag-day and `SPELUNK_SERVER_KEY` still overrides
  for CI and headless use.

- **`POST /v1/projects/{id}/index/embed` now returns raw bytes instead of JSON.**
  The embedding response body is `application/octet-stream`: raw little-endian
  `f32` bytes, row-major `[n_chunks × 896]`, in request order, with no per-row
  `chunk_id` framing (the client maps response row `i` to request chunk `i` by
  position). This drops per-element JSON serialize/parse on both server and CLI
  and shrinks the payload roughly 3× (3584 bytes per vector vs ~11 KB of JSON).
  `docs/openapi.json` is updated to match. (#441)

- **Cloud project slug auto-resolves to its server UUID.** When a team
  `server_url` routes projects by an internal UUID, a human `project_id` slug is
  now resolved to that UUID on first use via `GET /v1/projects` and cached in
  `.spelunk/cloud-project-id.lock`; a raw-UUID `project_id` is used directly, and
  a loopback/unset server is left untouched. The cache is invalidated
  automatically if the slug changes, and `SPELUNK_NO_SLUG_CACHE=1` forces a fresh
  lookup. This makes the human-readable `project_id` work transparently against
  cloud-api routing. (ADR-005, #428)

### Fixed

- **Retrieval quality: corrected grouped-query-attention (GQA) handling in the
  F2LLM embedder.** The first v0.9 build mis-handled the model's 16 attention
  heads / 8 KV heads (`n_rep = 2`), producing degraded embeddings. The fix
  changes the vectors F2LLM produces, so search results differ from (and improve
  on) that initial build. Re-indexing with the fixed embedder is required to get
  the corrected vectors. (#19, #441)

### Dependencies

- `tower-http` 0.6.11 → 0.7.0 (#431)
- `actions/checkout` 6 → 7 (CI) (#432)
- Refreshed `Cargo.lock` to latest semver-compatible versions; no new advisories
  (#433)

### Added

- **Windows CI matrix (`x86_64-pc-windows-msvc`).** `windows-latest` is now
  included in the `test` matrix, running `cargo build` + `cargo test`. The
  `check`/lint and `openapi-snapshot` jobs remain Ubuntu-only as they use
  POSIX tooling.

### Fixed

- **Indexed file paths are now stored OS-independently (forward slashes).**
  Indexing on Windows previously stored backslash separators (`src\lib.rs`)
  while every lookup uses forward slashes, so `spelunk chunks` / `cat-chunks`
  (and the `\` SQL-escape in `LIKE` lookups) found nothing for an indexed file
  on Windows. Root-relative paths are normalized to `/` on both indexing and
  lookup, making the on-disk index portable across operating systems. (#444)

## [0.8.3] — 2026-06-17

### Changed

- **Server instance_id now uses UUID v7 instead of v4.** The persistent instance ID (returned by `/v1/health` and stored in the server database) is now a time-ordered UUID v7 minted by the `uuid` crate (`Uuid::now_v7`) and persisted verbatim, so the high 48 bits encode the millisecond Unix timestamp of creation and the value is stable for the life of the database. It remains a standard 36-char UUID string. (#416)

- **Generate UUIDs with the `uuid` crate instead of hand-rolled helpers.** The bespoke `format_uuid_v7()` byte-formatter in spelunk-server and the `query_nonce_hex()` helper in the spelunk-cli server client (the latter previously the misnamed `uuid_v4_hex()`) have both been removed; the server instance id and the synthetic `query:<id>` embed chunk id are now produced by `Uuid::now_v7()`. (#416)

### Security

- **`explore` `read_file` confined to indexed files within the project root.**
  The `explore` tool loop previously read whatever path the LLM supplied, so
  adversarial instructions embedded in indexed source content could steer it
  into reading an arbitrary file the process could access (for example
  `~/.ssh/id_rsa` or a `../../etc/passwd` traversal) and returning the contents
  in the answer or step log. `read_file` now resolves every requested path
  against an allow-list: absolute, drive, UNC, and NUL inputs are rejected, `..`
  escapes are rejected lexically, the path must match an indexed file in the
  `files` table, and the canonicalized target must stay under the canonical
  project root (symlink backstop). A denied read is a recoverable tool result
  that echoes only the caller-supplied path, never a resolved path or file
  contents, and no longer aborts the session. (#403)
- **Storage SQL `IN (...)` queries are now fully parameterised and bind-limit
  chunked.** The four list-based query methods (`chunks_by_ids`,
  `graph_neighbor_chunks`, `mention_edges_for_chunks`, `chunks_mentioning_symbols`)
  previously assembled their `IN (...)` placeholder list at runtime with `format!`.
  No caller value was ever interpolated into the SQL text, so there was no active
  injection vector, but the hand-built placeholder construction was a latent
  hazard. A shared `placeholders(n)` helper now emits the placeholder list and all
  values are bound positionally via rusqlite params, so no caller-supplied id,
  name, or symbol can reach the SQL string. Each method also chunks its input at
  the SQLite bind-parameter limit (halved for the two-clause `graph_neighbor_chunks`)
  and merges results with unchanged semantics, removing the prior cap on input
  list size. Internal change only; no user-facing behaviour change.
  ([#405](https://github.com/spelunk-cloud/spelunk/issues/405))

### Internal

- Repaired the fuzz crate after the three-crate workspace migration and suppressed an upstream tree-sitter-sequel LeakSanitizer false positive so the fuzz CI job runs clean. ([#417](https://github.com/spelunk-cloud/spelunk/pull/417), [#418](https://github.com/spelunk-cloud/spelunk/pull/418))

## [0.8.2] — 2026-06-15

### Security

- **gix/gitoxide Dependabot alerts verified resolved (P1-2).** All 7 open Dependabot
  security alerts (6 high, 1 medium) for gix-related crates were already cleared by
  previous bumps (PRs #307, #327, #334). Cargo.lock contains patched versions:
  gix 0.84.0 (>=0.83.0), gix-fs 0.21.2 (>=0.21.1), gix-pack 0.71.0 (>=0.69.0),
  gix-transport 0.57.1 (>=0.56.0). `cargo audit` returns zero vulnerabilities
  (exit 0); the unmaintained `paste` crate surfaces as a non-failing warning
  (RUSTSEC-2024-0436), and the one advisory suppressed in audit.toml is
  RUSTSEC-2026-0097 (rand 0.10.0 via lopdf, no patched release yet).
  Affected advisories: GHSA-fr8x-3vfx-f45h, GHSA-pg4w-g64p-qwhj, GHSA-f26g-jm89-4g65,
  GHSA-p3hw-mv63-rf9w, GHSA-f89h-2fjh-2r9q, GHSA-x494-mj8g-cj27, GHSA-9857-6mw7-fq2m.

- **`spelunk memory add` blocks the entire write on secret detection.** When a
  secret is detected at input time, both the SQLite backend write and the
  git-notes write are now aborted — no partial write occurs. Previously only
  the git-notes path was guarded; the SQLite path could still persist a
  credential-containing entry. (#344)

### Added

- **Cross-project memory visibility (`spelunk memory search|list|context`).** When
  projects are linked with `spelunk link`, `memory search`, `memory list`, and
  `context` now also query each linked project's memory store and surface
  `locked` or `cross-project`-tagged `decision` and `requirement` entries
  alongside local results. Each cross-project result is tagged with its source
  project (`[from: <project>]` in text mode; `source_project` /
  `source_project_path` fields in JSON) so conflicting decisions remain
  attributable. `handoff` and `question` entries remain strictly project-local.
  (ADR-003)

- **`--local-only` flag for `memory search`, `memory list`, and `context`.**
  Suppresses the cross-project dep pass and queries only the primary project's
  memory store -- matching the existing `spelunk search --local-only` behaviour.
  (ADR-003)

- **`spelunk memory reconcile`** — imports notes from a local `spelunk-server`
  database (`server.db`) into the project's `memory.db` by content-hash dedup,
  without running the server. Reads `server.db` in read-only mode; never writes
  to it. Flags: `--source-db <path>` (override the default source path
  `~/.local/state/spelunk/server.db`), `--dry-run` (report candidates without
  importing), `--all-projects` (reconcile every project slug found in
  `server.db`), and `--format json` (machine-readable summary). Exits with code
  0 when there is nothing to import. (#391, ADR-004 follow-up)

### Fixed

- **SQLite LIKE queries now escape metacharacters in file paths and symbol names.** File
  paths and symbol names containing `%` or `_` were causing over-matching in
  `file_paths_under`, `chunks_for_file`, `symbol_history`, and `stale_specs` queries.
  An `escape_like()` helper now escapes these characters before binding, with
  `ESCAPE '\\'` on the SQL clause. ([#406](https://github.com/spelunk-cloud/spelunk/issues/406))
- **Batch summariser: use per-batch UUID delimiter to prevent chunk-content
  spoofing.** The batch summariser was using a static `===CHUNK {id}===` delimiter
  between chunks in the LLM prompt. Source code chunks whose content contained
  that string could spoof chunk boundaries and confuse the model. Now uses a
  per-batch UUID: `===CHUNK-{uuid}={id}===`. (#404)

- **`spelunk memory watch --help` now references `server_url` correctly.** The
  subcommand doc-comment previously said `requires memory_server_url` (the
  deprecated alias); corrected to `requires server_url`.
  ([#400](https://github.com/spelunk-cloud/spelunk/issues/400))

- **`explore` now appears in `spelunk --help`.** The `explore` subcommand was
  inadvertently hidden from the top-level help output; the hide attribute has
  been removed so users can discover it alongside the other subcommands.
  ([#400](https://github.com/spelunk-cloud/spelunk/issues/400))

### Dependencies

- `openssl` 0.10.80 → 0.10.81 (#408)
- `openssl-sys` 0.9.116 → 0.9.117 (#410)
- `regex` 1.12.3 → 1.12.4 (#409)

---

## [0.8.1] — 2026-06-10

### Fixed

- **`spelunk search` honors auto-discovered loopback server.** Explicit
  `--mode semantic`/`--mode hybrid` no longer error with "requires
  spelunk-server", and `--mode auto` no longer silently falls back to
  ast-grep, when no `server_url` is configured but a local `spelunk-server`
  was auto-discovered via the loopback probe (the default v0.8.0 UX).

- **Native embedder: fixed memory spike, CPU saturation, and index timeout.**
  Indexing large projects no longer triggers a ~20 GB memory spike, ~750%
  CPU usage across 31 threads, or HTTP timeouts during the embed phase.
  Adds a new `--embed-threads` CLI arg (default 4, env
  `SPELUNK_EMBED_THREADS`). Verified: 124-file / 1330-chunk index completes
  in 7m30s with stable ~3.5 GB memory and ~350-400% CPU.

- **Native embedder: reduced CoreML activation footprint and added compiled-model
  cache** for hardware EP builds (`embed-coreml` / `embed-xnnpack` /
  `embed-directml`), cutting peak memory from ~4 GB to ~1 GB and avoiding
  CoreML recompilation on every server start. These hardware EP features
  remain experimental and are not recommended over the default CPU EP — see
  `docs/server.md`.

### Security

- **Auto-spawned `spelunk-server` now binds to `127.0.0.1` only.** Previously
  the server started by `spelunk init` / `ensure_server_running` defaulted to
  `0.0.0.0`, making the unauthenticated local server LAN-reachable.

### Added

- **`spelunk status`/`check --format json`** now include a `memory_backend`
  field (`"sqlite"`, `"remote"`, or `"git-notes"`); `spelunk status` text mode
  shows a "Memory backend: <kind>" line. (#308)

### Changed

- **NDJSON terminology renamed to JSONL** throughout the CLI, docs, and tests.
  The `--format` flag value `ndjson` is now `jsonl` for `search`, `graph`, and
  `memory` commands. (#348)

- **Internal refactor:** `storage::remote` and `storage::git_notes` split into
  module directories to stay under the 400-line file limit. No public API
  changes.

- **Homebrew tap moved to a separate repo** (`spelunk-cloud/homebrew-spelunk`);
  the release workflow now publishes the formula there directly.

---

## [0.8.0] — 2026-06-08

### Breaking changes — migration required

**All AI inference commands now route through `spelunk-server`.**

The following commands previously called LM Studio (or another
OpenAI-compatible endpoint) directly via `api_base_url`. They now require a
running `spelunk-server` reachable at `server_url` in your config:

| Command | Previously needed | Now needs |
|---|---|---|
| `spelunk explore` | `api_base_url` + `llm_model` | `server_url` |
| `spelunk search` (semantic/hybrid) | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory search` (semantic) | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory timeline` | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory add` (auto-embed) | `api_base_url` + `embedding_model` | `server_url` (optional, degrades gracefully) |
| `spelunk index` (embed phase) | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk index` (summaries) | `api_base_url` + `llm_model` | `server_url` |
| `spelunk plumbing embed` | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory harvest` | `api_base_url` + `llm_model` | `server_url` (unchanged since #310) |

**Migrating from `lm_studio_url` / `api_base_url`:**

If you previously ran a local LM Studio and set `api_base_url` in your config,
you now need to run `spelunk-server` in front of it:

```toml
# ~/.config/spelunk/config.toml

# Old config (no longer used for inference):
# api_base_url = "http://127.0.0.1:1234"

# New config:
server_url = "http://127.0.0.1:7777"   # spelunk-server address
project_id = "your-org/your-project"   # required when server_url is set
```

Start `spelunk-server` and point it at your LM Studio instance:

```sh
spelunk-server \
  --embedding-url http://127.0.0.1:1234 \
  --embedding-model text-embedding-embeddinggemma-300m-qat \
  --llm-url http://127.0.0.1:1234 \
  --llm-model google/gemma-3n-e4b \
  --port 7777
```

Commands that do **not** need inference (parse, graph, FTS search, status,
memory list/show/archive) continue to work offline without `server_url`.

### Changed

- **`spelunk-core` no longer contains embedding or LLM implementations.**
  `OpenAiCompatEmbedder`, `OpenAiCompatLlm`, and `backends.rs` have been
  removed from `spelunk-core`. The `EmbeddingBackend` and `LlmBackend` traits
  remain in `spelunk-core` for use by `spelunk-server`'s `AppState`. (#260, #312)

- **Capability module moved from `spelunk-core` to `spelunk-cli`.** The tier
  detection logic (`get_tier`, `require_tier1`) is now internal to the CLI
  binary. Nothing outside spelunk-cli should depend on it. (#312)

---

## [0.7.1] — 2026-05-27

### Added

- **`spelunk-server` HTTP API** — Axum-based REST server with AuthProvider
  trait, `/v1/embed`, `/v1/explore`, `/v1/plan` endpoints, and an OpenAPI spec
  committed alongside the binary. Server-side embedding is optional; pass
  `SPELUNK_EMBEDDING_URL` to enable it. Prompt-injection patterns are rejected
  server-side before storage. (#261, #221, #222)

- **`spelunk status --format json`** — stable machine-readable schema for
  status output, suitable for CI dashboards and agent health checks. (#269)

- **Heuristic convention extraction** — `spelunk index` now detects and stores
  project conventions (naming patterns, async style, test coverage, doc
  coverage) derived from the AST. Results are surfaced in `spelunk context`
  output. (#268)

- **Compatibility tier model** — `spelunk check` reports a capability tier
  (Local / Embedded / Full) so agents can adapt their strategy to the available
  inference backend at runtime. (#259)

- **`spelunk graph --live`** — passes the query to ast-grep as a fallback when
  the indexed call graph has no results, giving live symbol resolution for
  unindexed or recently changed code. (#216)

### Changed

- **3-crate Cargo workspace** — the codebase is now split into `spelunk-core`
  (library), `spelunk-cli` (binary), and `spelunk-server` (binary + lib) under
  a shared workspace root. `CLAUDE.md` and `README.md` updated accordingly.
  (#220)

- **`gix` status API** — subprocess calls to `git status` replaced with
  `gix::status` API, removing a shell dependency and improving reliability
  inside IDE integrations. (#215)

- **`spelunk explore` now requires a configured server** — the command is gated
  behind the Tier 2/3 capability check (`server_url` must be set and reachable).
  The previous check for `llm_model` has been removed in line with decision #47
  (no LLM inference in the CLI without a server). Run `spelunk status` for
  guidance if the command is unavailable.

### Fixed

- `spelunk-server` OpenAPI spec gaps: `SearchRequest` missing `text` field,
  JSON error shapes aligned to `application/json` responses, CI step added to
  gate spec drift. (#288)

- `.spelunk` symlink replaced with runtime worktree-root resolution, fixing an
  infinite-symlink issue when indexing inside a git worktree. (#266)

- `spelunk memory harvest` now swallows per-entry errors and continues rather
  than aborting the entire run on a single bad entry. (#270)

### Dependencies

- `serde_json` 1.0.149 → 1.0.150
- `tree-sitter` 0.26.8 → 0.26.9
- `tower-http` 0.6.10 → 0.6.11

---

## [0.7.0] — 2026-05-17

### Added

- **`spelunk context`** — new agent session entry point command that surfaces
  index health, recent memory, and open questions in a single structured
  output, making it easier for AI agents to orient at the start of a session.

- **`spelunk memory harvest --source entire`** — mines Entire.io checkpoint
  files (stored on the `refs/entire/checkpoints/v1` branch) for decisions,
  notes, and requirements. The structured `Summary` in each checkpoint is used
  directly; an LLM fallback is used only for checkpoints that lack one. Secret
  scanning is applied on all paths so credentials are never stored.

- **`GitMetaBackend`** — second memory backend backed by `git-meta-lib`, available
  alongside the existing git-notes and server backends.

- **`git-notes` memory backend** (`GitNotesBackend`) — store and retrieve memory
  entries using git's native notes mechanism, with no server or external database
  required. Supports optional embedding for semantic memory search. `NoteRecord`
  now carries a `schema_version` field for forward-compatible detection of future
  record formats.

- **Benchmark suite** (`bench/`) — five benchmarks now ship with the repo to
  measure and document retrieval quality and performance: Decision Archaeology
  (memory recall), Cross-Session Handoff (agent continuity), SWE-bench
  (patch resolution rate via Docker harness), Code-Graph (grep vs search vs
  graph), and Perf-at-Scale (indexing and search latency at 50k–500k LOC).
  A benchmarking report (`tmp/benchmarking-report.md`) summarises current
  results.

### Changed

- **Removed legacy commands** — `ask`, `plan`, `spec`, `snapshot`, `history`,
  and `verify` subcommands have been removed. The core spelunk workflow is now
  index-free by default; docs and SKILL.md have been updated to reflect this.

- **`gix::discover` replaces `git rev-parse`** — subprocess calls to
  `git rev-parse` for repository discovery are replaced with `gix::discover`,
  removing a shell dependency.

- **Dependency updates** — `gix` bumped to 0.83.0 (fixes worktree exclude
  handling so `.gitignore` rules are correctly respected in git worktrees);
  `git-meta-lib` bumped to 0.1.10 with API adaptation.

### Fixed

- `GitNotesBackend` unsupported methods now return a typed `BackendUnsupported`
  error instead of panicking.

- `GitNotesBackend` list capped at 500 entries, preventing an O(n) hang on
  repositories with many notes.

- `spelunk index` no longer creates a self-referential `.spelunk` symlink when
  run inside a git worktree checked out at the same path as the main repo.

### Security

- Server audit checklist (`docs/security/`) added as groundwork for the upcoming
  v1.0 server release.

---

## [0.6.0] — 2026-04-26

### Added

- **LinearRAG two-stage retrieval** — a new graph-diffusion retrieval algorithm
  combining personalised PageRank with full-text pre-filtering. Now the default
  for `spelunk search`; multi-hop recall is +33.5 % over the previous baseline
  at ~1.9× median latency. Compound indexes and in-memory Stage 1 propagation
  keep latency within acceptable bounds.

- **`.spelunkignore` files** — place a `.spelunkignore` file anywhere in your
  project tree to exclude paths from indexing, using the same format as
  `.gitignore`.

- **`antipattern` memory kind + `spelunk memory failures`** — record failure
  patterns and anti-patterns as first-class memory entries. `spelunk memory
  failures` lists them; `spelunk memory harvest` can extract them from session
  history.

- **Expanded fuzzer coverage** — fuzzer targets now cover secrets, chunker,
  `escape_xml`, JSONL, and history entry parsing.

### Fixed

- OOM guard added to the parser; `doc_comment` nodes are skipped during AST
  walking to avoid memory exhaustion on files with large doc blocks.

- `spelunk check` was made fully async, fixing a panic caused by calling
  `block_on` inside an existing async context.

- PageRank dangling-node indices are now precomputed before power iterations,
  improving performance on sparse graphs.

---

## [0.5.0] — 2026-04-21

### Added

- **Unix plumbing/porcelain architecture** — 8 new `spelunk spelunk` plumbing
  subcommands emit machine-readable JSONL to stdout and use conventional exit
  codes (0 = ok, 1 = no results, 2 = error). All porcelain commands now accept
  `--format text|json|jsonl` for structured output in scripts and agents.
  Plumbing commands: `cat-chunks`, `embed`, `graph-edges`, `hash-file`, `knn`,
  `ls-files`, `parse-file`, `read-memory`.

- **`spelunk memory harvest --source claude-code`** — mines Claude Code session
  history files (`.claude/projects/*/sessions/*.jsonl`) for decisions, notes,
  and requirements; deduplicates against already-stored entries; stores the
  results directly in the memory index.

- **`intent` memory kind** — agents record work-in-progress intent entries so
  collaborating agents (and humans) can see what is actively being changed.
  `spelunk check` now shows active agent sessions alongside the index health
  summary, and warns when any intent's linked files overlap with files recently
  modified in the current worktree.

- **Server-side conflict detection** — `spelunk-server` runs a KNN similarity
  search before storing each new memory entry; entries that closely contradict
  an existing active entry are flagged with a `contradicts` edge and the HTTP
  response includes a `409 Conflict` status with the conflicting entry IDs.
  A `--conflict-threshold` flag controls the cosine-distance trigger.

- **`spelunk memory since` / `spelunk memory watch`** — incremental memory feed
  (`since`) and a long-running SSE stream (`watch`) for agents that want to be
  notified of new memory entries in real time. _(coming soon in 0.5.x — not
  yet merged as of this release)_

- **Benchmark scripts** (`bench/`) for evaluating search quality across
  indexing configurations.

### Changed

- `--format text|json` standardised across all porcelain commands (`ask`,
  `explore`, `search`, `graph`, `memory list`, `memory search`). The legacy
  `--json` flag is kept as a hidden deprecated alias.

- `storage/memory.rs` split into focused sub-modules
  (`storage/memory/`, `storage/db/`) to reduce file size and improve
  navigability, as part of the broader Unix-architecture refactor.

### Fixed / Security

- **XML escaping in LLM prompts** — spec titles and paths interpolated into
  `<spec_context>` blocks are now escaped with `escape_xml()`, closing a
  prompt-injection vector.

- **Expanded secret scanner** — `src/indexer/secrets.rs` now recognises
  OpenAI, Anthropic, and Stripe API keys; npm automation tokens; and database
  connection URLs containing inline credentials. Patterns compile once via
  `OnceLock`.

- **Atomic memory transactions** — `NoteStore` archive and supersede operations
  now run inside a single SQLite transaction; partial writes on crash are no
  longer possible.

- Resolved all security-audit findings from `cargo audit` (#136, #137, #138,
  #145) by upgrading affected dependency versions.

---

## [0.4.1] — 2026-03-21

Initial public release.
