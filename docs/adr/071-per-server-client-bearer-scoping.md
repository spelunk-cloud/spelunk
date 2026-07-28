# ADR-071: Per-server scoping of the client bearer credential

**Date:** 2026-07-16
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** operates strictly inside
[ADR-056](056-oss-server-tenancy-model.md)'s tenancy model (a spelunk-server
instance is a single trust domain, its shared key is the boundary, and
isolation between groups is achieved by running separate instances) and does
not reopen it. Takes [ADR-066](066-native-tls-in-spelunk-server.md)'s
transport as given: the team server is spelunk-server itself over HTTPS plus
an API key, with no reverse proxy in front. This ADR is about how the *client*
holds and selects among those API keys once ADR-056's topology gives one
developer more than one of them.

## Context

### The topology multiplies keys; the client holds one

ADR-056 settled that a spelunk-server instance is one trust domain and that
two groups needing isolation from each other run **separate server
instances**, each with its own key and its own database. That decision is not
reopened here, and it has a client-side consequence nothing has absorbed yet:
a developer who works on two projects backed by two servers legitimately holds
two keys, one per instance.

The client cannot represent that. The credential resolution in `Config::load`
(`crates/spelunk-core/src/config.rs:573-591`) resolves exactly one flat
bearer, with this precedence (highest first):

1. `SPELUNK_SERVER_KEY` environment variable
2. `[auth].access_token` from `spelunk login` (the WorkOS cloud path)
3. the secret-store `server_key` entry (keychain by default, owner-only file
   fallback when headless)
4. a `server_key` from the committed project-level `.spelunk/config.toml`

Nothing in that chain is keyed to `server_url`. Whichever value wins is
attached to whatever server the resolved `server_url` names, so a two-server
developer either juggles `SPELUNK_SERVER_KEY` per invocation or lets the wrong
key hit the wrong server and gets a 401. The topology ADR-056 recommends is,
on the client, an env-var discipline problem.

### The key has no front door, and two documented back doors

Three further facts about the current state, each verified against the tree:

- **`save_server_key` has no production caller.**
  `crates/spelunk-core/src/config.rs:689` defines the function that persists
  the bearer into the secret store, and nothing outside its own tests calls
  it. Its doc comment says it is "the token `spelunk login` persists", which
  is wrong: `login` persists the `[auth]` token pair and never touches it
  (only `logout` reaches the entry, via `remove_server_key`,
  `crates/spelunk-cli/src/cli/cmd/logout.rs:14`). There is no key-set command.

- **The documented way to set the key is a plaintext file edit.**
  `docs/server.md:159-164` instructs pasting the shared key into the personal
  `~/.config/spelunk/config.toml` as plaintext. A one-time migration in
  `Config::load` (`config.rs:524-541`) then quietly moves it into the secret
  store and strips the file. So the plaintext edit is the de facto set-key
  flow: the migration path is the front door, entered backwards.

- **The committed project file accepts a credential.** `docs/server.md:143`
  tells each developer to add `.spelunk/config.toml` at the project root and
  commit it, and `ProjectConfig` (`config.rs:256-266`) accepts a `server_key`
  field in that file, tier 4 of the precedence above. The struct's own doc
  comment says it contains "no secrets", one field above a field whose comment
  concedes it is a shared API key that is "acceptable if the server is behind
  a VPN/firewall". A credential in a committed file is in the repo's history
  for good, visible to anyone with repo access whether or not they should hold
  the key, and rotatable only by rewriting a file everyone has.

So the flat key is simultaneously too coarse for the recommended topology and
held in places a credential should not be. Both problems have one fix: give
the credential a real home, keyed by the server it belongs to.

## Decision

**Store a per-origin key map in the secret store as a single entry; resolve
the bearer for a given `server_url` through it by branching on credential kind
rather than probing every tier; give the key a real command surface
(`spelunk auth set-key`, `spelunk auth list-servers`, and a scoped
`spelunk logout --servers` / `--server <url>`); remove `server_key` entirely
from the committed project config; and migrate the legacy flat key into the
map instead of reading both indefinitely.**

### D1 – one secret-store entry holding a per-origin key map

A single new secret-store entry, `(service = "spelunk", user =
"server_keys")`, whose opaque string payload is a JSON object mapping origin
to key:

```json
{ "https://spelunk.internal.example.com": "sk-...", "https://other.example.net:8443": "sk-..." }
```

The map key is the **normalized origin** of the resolved `server_url`: scheme,
host, and port (explicit, with the scheme default applied), nothing else. Path,
query, trailing slash, and host case do not participate. Origin is the right
granularity because it is the trust-domain granularity: ADR-056 makes the
instance the boundary, and an instance is addressed by an origin.

**One entry, not one entry per host.** The keyring layer stores each secret as
its own keychain item (`(service = "spelunk", user = <key>)`,
`crates/spelunk-core/src/config/secret_store.rs`), and on macOS each distinct
keychain item prompts for access separately, even after "Always Allow" has
been granted for another item under the same service. Per-host items would
turn adding a second server into a second permission dialog for every binary
that reads keys. One item means one grant covers the whole map, and adding a
server never re-prompts.

**The `SecretStore` trait does not change.** It stays a `get`/`set`/`delete`
over opaque strings; the JSON encoding and origin normalization live entirely
in the config layer above it. The trait's opacity is what keeps the keychain,
file-fallback, and any future backend interchangeable, and a map-shaped
payload is exactly the kind of thing opacity is for.

### D2 – resolution is per resolved `server_url`, branched by credential kind

The bearer for a request is resolved against the `server_url` the request
will actually go to. Resolution first decides *which kind* of credential this
`server_url` calls for, then looks only in the store(s) that kind uses: it
does not probe every tier and take whichever answers first, because a flat
probe-all chain is what produced the prompt-count problem below.

**The kind is decided by the resolved `server_url`'s origin, not a new config
field.** If the origin matches the cloud origin (`DEFAULT_CLOUD_URL`,
`crates/spelunk-cli/src/cli/cmd/auth_api.rs:28`, `https://api.spelunk.cloud`
by default, overridable with `SPELUNK_CLOUD_URL`), this is the **cloud
kind**; any other origin is the **server-key kind**. No explicit `Config`
field (e.g. a `use_cloud` flag) is introduced to make this distinction:
`server_url` already carries the answer. This is the same signal
`Config::resolve_mode`'s serde default already uses to distinguish "talking to
a configured server" from "fully local" (`config.rs:670-675`), and cloud
memory sync already runs through this same `server_url` field rather than a
separate cloud code path (`crates/spelunk-cli/src/cli/cmd/memory/sync.rs:60,75`;
`crates/spelunk-core/src/storage/mod.rs:104`). A second field would duplicate
information the origin already carries and could drift from it, so the answer
to "do we need a `Config(use_cloud)`" is no: branching on origin gives the
same disambiguation for free.

- **Cloud kind** (origin is the cloud origin), highest first:
  1. `SPELUNK_SERVER_KEY` environment variable, unchanged and still a
     per-invocation override.
  2. `[auth].access_token`, unchanged; its own refresh lifecycle, untouched
     by this ADR.
  The server-key map and the legacy flat entry are never consulted for this
  kind; a cloud request has no reason to touch either secret-store item.
- **Server-key kind** (any other origin), highest first:
  1. `SPELUNK_SERVER_KEY` environment variable, unchanged.
  2. `server_keys[origin]`, the map from D1, looked up by the normalized
     origin of the resolved `server_url`.
  3. The legacy flat secret-store `server_key` entry, but only transiently:
     the first time this tier answers for an origin, it is migrated into the
     map and then removed, rather than read indefinitely alongside it (see
     "Migration and back-compat" below).
  `[auth].access_token` is never consulted for this kind.

This answers "wouldn't this mean we're back to two keychain prompts?": the
previous draft's flat four-tier chain could touch the map, then the legacy
entry, then (for a request that also happened to have a cloud login sitting
in `[auth]`) up to three secret-store items for one request, regardless of
which one the request actually needed. Branching by kind first bounds a given
request to the items its own kind uses, and the migration step folds the
server-key kind's two items down to one after its one-time run, so steady
state is one secret-store item touched per request, which is the single
prompt D1's "one entry" design was meant to guarantee, now actually delivered
end to end rather than undercut by a second, indefinitely-read legacy item.

This lands as a `Config::bearer_for(server_url)` lookup rather than a field
populated once at load time. That placement is deliberate: concurrent work on
reducing macOS keychain prompts is moving secret-store reads onto a lazy
resolution seam, so that commands which never talk to a server never touch
the keychain, and per-URL resolution has to sit on the same seam or it would
re-introduce an unconditional keychain read at load. The two changes are
separate deliverables but share the seam, and this ADR's resolution order is
the contract for what `bearer_for` returns regardless of when it is called.

Tier 4 of the *old* chain, the committed project-file `server_key`, is
removed rather than re-scoped. D4 records that as its own decision.

### D3 – the key gets a command surface

Three commands, of which two are new:

- **`spelunk auth set-key --server <url>`** stores a key for a server. The
  key is read from stdin or an interactive prompt, **never** from argv: a
  positional or flag-valued secret lands in shell history and in `ps` output,
  which is the same class of leak D4 closes for the committed file. The URL
  is normalized to its origin before storage, so `set-key` and resolution
  cannot disagree about spelling.
- **`spelunk auth list-servers`** prints the origins present in the map, and
  whether a legacy flat key also exists. It never prints key material, not
  even truncated: a listing surface that shows secret prefixes trains users
  to have secrets on screen.
- **`spelunk logout`** (existing) is scoped down to exactly the credential it
  exists to undo: the `[auth]` cloud token pair from `spelunk login`, plus any
  plaintext remnant of that pair in the personal config. It stops
  unconditionally clearing the legacy flat `server_key` entry too (its
  behavior before this ADR, `crates/spelunk-cli/src/cli/cmd/logout.rs:14`),
  and does not start also clearing D1's map. Logout exists to fix a
  cloud-login problem (its own doc comment: "clear stored spelunk.cloud
  credentials"); automatically deleting a self-hosted server credential as a
  side effect of fixing an unrelated cloud-access issue is exactly the
  behavior the founder review rejected: a developer recovering from a broken
  cloud login should not silently lose the server key(s) they use on other
  projects. Clearing a server-key credential is now its own explicit action:
  - **`spelunk logout --servers`** clears the whole map and the legacy flat
    entry (the "remove everything" behavior the previous draft made
    automatic, now opt-in).
  - **`spelunk logout --server <url>`** clears only that one origin's
    credential (map entry, or the legacy entry if that origin is still
    served by it).
  Bare `logout`, when server keys are present, prints how many are stored and
  names the flag that removes them; it takes no destructive action on them
  itself.

This is the first production caller of the `save_server_key`-shaped
persistence path, which until now existed only for its tests, and it retires
the plaintext-file edit as the documented way to install a key. The stale doc
comment on `save_server_key` (claiming `spelunk login` persists it) is
corrected as part of this work.

### D4 – `server_key` is removed entirely from the committed project config

`ProjectConfig` (`config.rs:256-266`) drops the `server_key` field outright.
Nothing in the client reads a `server_key` out of `.spelunk/config.toml` any
more, at any tier. A file that still has the line gets **no load-time
warning and no special handling**: serde silently drops the unrecognized
field, the same way the removed `memory_server_*` aliases are silently
dropped today (`config.rs:1209` onward, regression tests for that removal),
and the load proceeds as if the field had never existed.

This is a stronger stance than the warn-and-ignore this ADR originally
proposed, taken on the founder review's case for it: anyone who has not
already hit this is, by construction, still in the onboarding journey for
client configuration: no supported path other than the (now-corrected) docs
ever told them to put `server_key` in a committed file, so they need to
re-read the docs, not receive a runtime nudge from a code path we would
otherwise carry indefinitely. A load-time warning is itself permanent code
whose only job is to describe a field that no longer does anything; removing
the read path removes the warning's reason to exist too. It also closes a
subtler risk the founder review flagged: keeping a "warn but still parse"
branch invites a developer in a rush to copy or recreate a
`~/.config/spelunk/config.toml`-shaped `server_key` line into the project
file to make the warning go away, reconstructing the very committed
credential this ADR exists to eliminate. No parsing, no warning, no such
temptation.

This is still a deliberate breaking change taken inside the pre-v1.0 window,
with the same `memory_server_*` precedent (rejected-with-guidance rather than
silently honored) as before; "guidance" now lives in the docs and release
notes rather than at load time, per the Migration section below. The other
shareable fields (`server_url`, `project_id`, `server_ca`) are unaffected;
they are what the committed file is for.

The operator-facing consequence (*rotate a key that was ever committed,
because git history retains it regardless of what the client does with the
field*) is not something a silent removal can say at runtime. It is said
once, explicitly, in the same-release docs rewrite (see "Migration and
back-compat" and "Security implications" below), which is the only place
left to say it once the load-time path is gone.

### Migration and back-compat

- **The legacy flat entry is migrated into the map, not read indefinitely
  alongside it.** An earlier draft kept the legacy entry as resolution tier 3
  forever, re-reading it on every server-key-kind request that reached that
  tier, which is exactly what produced the "two keychain prompts" problem
  D2 now answers, and is the kind of permanent dual-support the founder
  review pushed back on: *"I would prefer if we migrate it, we can deprecate
  the migration at create time... that gives people an upgrade path... but
  doesn't burden us with dead code in the long run."* Instead: the first time
  server-key-kind resolution runs for *any* origin and finds a legacy flat
  entry with no map entry yet for that origin, it writes
  `server_keys[origin] = <legacy value>` and deletes the legacy entry, in one
  step, the same shape as the existing plaintext-file migration
  (`config.rs:524-541`), which already moves a value between stores
  transparently and once. A one-line stderr notice explains what happened and
  names `auth set-key` for any other origin the user needs.

  This is safe even for a user who has been keeping two servers alive purely
  by juggling `SPELUNK_SERVER_KEY` per invocation: before this change, the
  legacy flat entry answered as a fallback for *every* unmatched origin, so a
  second, unmigrated origin could silently receive the first origin's key.
  After the migration runs, that second origin instead gets a clear "no key
  stored for this origin, run `spelunk auth set-key --server <url>`" error.
  Failing closed with an actionable message is strictly safer than the silent
  cross-origin reuse it replaces.

  **This migration path, and the legacy tier it retires, are deprecated as of
  this ADR** and are removed together no earlier than **v1.1**, one release
  after the v1.0 this ships in, which is enough of a window for a
  single-server user to be migrated automatically the first time they run
  any command against their server post-upgrade, without this codebase
  carrying the fallback-read indefinitely.
- **No origin-guessing is needed at migration time.** `bearer_for(server_url)`
  already knows the origin it is resolving for when it runs the migration
  above, so there is no ambiguity about which server a bare legacy key
  belongs to. The earlier concern ("teaching the migration to guess an
  origin for a bare key would require guessing which server it belongs to")
  applied to migrating it eagerly at load time, before any origin was in
  play; migrating it lazily, at first per-origin resolution, does not have
  that problem, because by then the origin is a request parameter, not a
  guess.
- **Docs land in the same PR as the code, not separately.** The
  client-configuration sections of `docs/server.md` and `docs/self-hosting.md`
  are rewritten to describe `auth set-key`, `auth list-servers`, and
  `logout --servers`/`--server <url>` in the same change that ships this
  ADR's code, not deferred to a later docs pass. Shipping a release that
  tightens credential-handling security posture without documenting the new,
  correct way to do it would leave the old (worse) way as the only thing
  users can find. This rewrite is also where the operator instruction the
  founder review asked for lives: anyone who had a `server_key` in a
  committed `.spelunk/config.toml` should treat it as exposed and rotate it
  on the server side, because git history retains it regardless of what the
  client does with the field (D4).

## Non-goals

- **Not reopening ADR-056.** The server-side model stays: one instance, one
  trust domain, one shared key, isolation by separate instances. This ADR
  changes only how the client holds the keys that model hands out. Per-project
  or per-principal ACLs on a single instance remain out of scope and deferred.
- **Not a reverse proxy or any new server surface.** ADR-066 stands; nothing
  here touches the server at all.
- **Not touching the `[auth]` cloud token path.** `spelunk login`'s WorkOS
  tokens keep their own storage, refresh, and precedence position.
- **Not per-project keys.** The map is keyed by origin, not by project.
  Two projects on one server share that server's key, which is exactly
  ADR-056's model. A key map keyed by project would imply an authorization
  granularity the server does not have.
- **Not credential rotation, expiry, or multiple keys per origin.** The map
  holds one current key per origin. Rotation is `auth set-key` with the new
  value.

## Consequences

- **Two-server workflows work concurrently, with no env juggling.** Each
  invocation resolves the key for the server it is actually talking to. The
  env var remains available as an override, but stops being the only way.
- **The credential has a front door.** `auth set-key` replaces "paste it into
  a config file and let the loader migrate it", and the key never transits
  argv, shell history, or a committed file on the supported path.
- **Repos with a committed `server_key` are no longer read at all.** The
  field is dropped from `ProjectConfig` (D4); a file that still has it keeps
  working for its other fields and silently drops that one, exactly like the
  removed `memory_server_*` aliases: no runtime warning. This is the one
  breaking edge, taken deliberately pre-v1.0. Removing the field from the
  file, and rotating the key it exposed (since git history retains it
  regardless of removal), are the operator's follow-up; the same-release
  docs rewrite says so explicitly, because nothing at runtime can.
- **A second resolution input exists.** The bearer now depends on
  `server_url`'s origin, not just on which stores hold values. Debugging
  "which key was sent" gains a step, which `auth list-servers` plus the
  deterministic, kind-branched precedence in D2 is designed to keep cheap.
- **`logout` clears exactly what it always cleared, no more.** The `[auth]`
  cloud token pair, unconditionally, on bare `logout`. Clearing a
  self-hosted server credential (the map, the legacy entry, or both) is now
  `--servers` or `--server <url>`: an explicit action, not an automatic
  consequence of fixing a cloud-login problem.
- **Docs land with this change, not after it.** `server.md` and
  `self-hosting.md`'s client-configuration sections are rewritten in the same
  PR, so there is no window where the shipped behavior and the documented
  behavior disagree.

## Security implications

- **The committed-file credential path is closed.** A shared bearer in a
  committed `.spelunk/config.toml` was readable by anyone with repo access
  and preserved forever in history. After D4 the client never reads it, so
  the file stops being a live credential carrier; existing history exposure
  is a rotation matter for operators, stated explicitly in the same-release
  docs rewrite (D4) rather than in a runtime warning.
- **No secret ever transits argv.** `auth set-key` reads from stdin or a
  prompt. `list-servers` prints origins only.
- **Key material stays in the secret store.** The map lives in the same
  keychain (or owner-only file fallback) as the flat entry it generalizes;
  no new storage location is introduced, and `config.toml` remains
  credential-free on the supported path.
- **Blast radius per key shrinks in the multi-server case.** With one flat
  key it was easy for the wrong server to be sent a valid key for a
  different, more privileged instance. Origin-scoped resolution means a key
  is only ever presented to the origin it was stored for; the env-var
  override remains the deliberate escape hatch and keeps its current
  semantics.
- **The trust model is unchanged.** The key still grants everything on its
  instance (ADR-056), and transport is still ADR-066's native TLS. This ADR
  neither strengthens nor weakens what a key can do; it changes where keys
  live and which one is sent.
