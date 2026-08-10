# Version skew

[Stability contract](stability.md) says what a surface promises within one
version. This document says what happens when the two ends are *different*
versions, which is the normal case rather than the exception.

The CLI talks to three server-side peers, and they drift independently:

| Peer | How it is reached | Why it drifts |
|---|---|---|
| **Loopback server** | auto-discovered on `127.0.0.1:7777` | The CLI starts and manages it, so it is normally the same version. It can go stale when a long-running daemon outlives an upgrade. |
| **Team `spelunk-server`** | explicit `server_url` | Upgraded on someone else's schedule. Skew is guaranteed, in both directions. |
| **cloud-api** | explicit `server_url` | Released independently of the CLI, so it can be ahead of any released CLI at any time. |

Drift can enter from any of the three independently. A newer CLI meets an older
server, and an older CLI meets a newer server, in the same week.

## The support window

| Pairing | Supported range |
|---|---|
| CLI *n* to team server | *n-1*, *n*, *n+1* |
| CLI to loopback server | same version |
| CLI to cloud-api | any; cloud-api evolves additively within `/v1/` |

The team-server window is one minor version in each direction. It is *not* a
promise that wider gaps fail, and in practice they often work: it is the range
that is tested, and therefore the range a break is treated as a bug in.

`GET /v1/health` carries the peer's real version in its `version` field.
`info.version` inside the OpenAPI spec is a placeholder (`0.1.0` regardless of
the build) and must not be used for this.

Read that as the version being *available*, not as it being *checked*. The CLI
does not parse `version` from the health body and does not compare it to its
own, so nothing below is enforced by a version comparison. Everything the CLI
does about skew it does structurally, by tolerating the shape of what arrived.

### Outside the window

Outside the window the CLI keeps working on a best-effort basis rather than
refusing to run. That is a deliberate choice and worth stating, because the
alternative reads as safer than it is: a hard version gate turns every
"upgrade the CLI before the server" ordering into a total outage, including for
the person who is upgrading the server. A soft failure that names the versions
is more recoverable than a hard one that is correct in principle.

Whether the CLI should ever refuse a pairing outright, and whether a mismatched
loopback server should warn or refuse, are open decisions. No such gate exists
today, and this document is a policy statement rather than a description of one.

What holds instead:

- Every field of the health body degrades **on its own**. A value this build
  cannot read costs that field and nothing beside it, where it used to discard
  the entire body. That applies to the members of `limits` too: a peer
  advertising `max_batch_chunks: 16` next to an unreadable sibling keeps the
  16. Reading the object all-or-nothing would discard the 16 and leave the
  client planning around its own maximum of 256, so an all-or-nothing degrade
  is the more permissive choice here, not the safer one.
- A peer that omits `limits` altogether is treated as enforcing the legacy
  profile, and that is **two separate fallbacks, not one**. On the time axis
  the CLI assumes a 30s server-side `/index/embed` budget and targets batches
  of 20s, two thirds of it. On the chunk axis it plans around 256 chunks per
  request. Check the axes separately: reading "assume the legacy profile" as a
  single conservative default is exactly what hid the chunk axis, where the
  fallback is this CLI's own maximum and carries no margin at all.

  Both numbers were checked against the releases that actually omit `limits`,
  v0.8.0 through v0.9.2. Checking them against the current server would have
  proved nothing: it advertises `limits`, so it is never the peer this fallback
  describes. On the chunk axis every one of those releases carries
  `const MAX_BATCH: usize = 256` and answers `413` above it, so 256 is what
  those peers genuinely enforce rather than a cautious guess. On the time axis
  the 30s is the blanket request timeout v0.9.2 applies with no `/index/embed`
  exemption; v0.8.0 through v0.9.1 impose no wall-clock budget at all, so
  assuming one there is conservative rather than measured.
- Unknown fields, and unknown values in an open enum, are ignored rather than
  failing the response that carried them.
- A body that is not a health object at all produces a warning naming the peer
  URL and the consequence, not a panic and not a silent empty result. See
  [When a capability goes missing](#when-a-capability-goes-missing) for how to
  see it: these are `warn`-level log lines, and the CLI logs at `error` unless
  `RUST_LOG` says otherwise.

## What evolution is allowed

The `/v1/` rules in [Stability contract](stability.md) already cover this:
additive only within a major version. Version skew is what makes that rule
load-bearing rather than tidy, so it is worth restating the direction each side
has to tolerate:

- **A newer peer sends fields the CLI has never seen.** The CLI must ignore
  them. This includes new values in an existing enum field, which is the case
  most likely to be mistaken for a parse error: an unrecognised value must
  degrade that one field, never the whole response.
- **An older peer omits fields the CLI expects.** The CLI must supply the
  documented default. Every optional in the health body already does this.

## When a capability goes missing

The health body is how the CLI learns what a peer can do, so a field it cannot
read shows up later as a capability that is quietly not there. `spelunk status`
is where you see the result: `capabilities`, `embedder_state`, and
`has_semantic_search` under `--format json`.

Until recently one unreadable field cost you all of them. Any single value the
CLI could not parse, anywhere in the body, made it discard the whole response
and fall back to treating the peer as a legacy plain-text server: semantic
search, index embed, and harvest all reported unavailable, every advertised
limit dropped, and nothing logged to say why. A newer server adding one enum
value, which the additive-only rule expressly permits, was enough to trigger it.

Now each field degrades on its own, and says so. The catch is that it says so at
`warn` level, and the CLI logs at `error` unless `RUST_LOG` is set, so the
warnings are there but off by default:

```
RUST_LOG=warn spelunk status
```

Two shapes of warning can appear:

- **One field could not be read.** The line names the field, the shape this
  build expected, and what the fallback costs. It deliberately does **not**
  print the value. `/v1/health` needs no authentication and its body is
  whatever `server_url` resolves to, so a field's contents are peer-controlled
  and unbounded; the received JSON kind (`a string`, `an object`) carries the
  diagnostic value without reproducing the value itself.
- **The body was not a health object at all.** The line names the peer URL, the
  consequence, and a bounded sample of what did arrive. This one is now the
  only route to the legacy fallback, because no individual field can reach it
  any more. In practice it means the URL is not a spelunk server.

If the capabilities you expect are missing and no warning appears, the peer
genuinely did not advertise them. That distinction, between a peer that said no
and a peer whose answer could not be read, is the whole point of the warnings.

## The memory read-endpoint envelope

The team server's three memory *read* endpoints wrap their result in an object,
never a bare array (ADR-076: a JSON response root must be an object):

| Endpoint | Shape |
|---|---|
| `GET /v1/projects/{id}/memory` (list) | `{ "entries": [...], "total": N }` |
| `POST /v1/projects/{id}/memory/search` | `{ "entries": [...], "total": N }` |
| `GET /v1/projects/{id}/memory/harvested-shas` | `{ "shas": [...] }` |

Servers before this change returned a bare `[...]` (or, for harvested-shas, a
bare `["sha", ...]`). That is a wire-shape change, so it is handled the way this
document handles every other skew: structurally, by tolerating what arrives, on
the side that can be changed.

- **Newer CLI to older server** is the common direction (a CLI upgrades ahead
  of a team server on someone else's schedule) and is covered in full. The
  team-server memory client (`storage/remote/wire_types.rs`) reads both shapes:
  an untagged reader accepts the object envelope from a current server and the
  legacy bare array from any server still inside the *n-1* window. Nothing about
  this direction breaks.
- **Older CLI to newer server** cannot be made to work by tolerance: an already
  released CLI only knows the bare array, and a single JSON body cannot be both
  an array and an object at once. This direction is therefore effectively gated
  on a minimum CLI version for these three endpoints. It is the less common
  direction, it is bounded to three read paths, and the CLI and team server ship
  in lockstep from one repo at one version, so the recovery is the same
  "upgrade the CLI" the rest of this document assumes.

The server emits the envelope *unconditionally* rather than negotiating the
shape per request. Serving the bare array to some callers would keep it reachable
forever, which is exactly the invariant ADR-076 exists to retire, and would
leave nothing for the wire-shape pin test to hold. The bounded one-direction
break is the accepted cost of making "the root is always an object" true rather
than aspirational.

Out of scope, and deliberately still a bare array: `GET …/memory/since?t=<epoch>`
keeps its documented legacy shape (its `?since_id=` cursor mode already returns
`{entries, count}`), and `GET /v1/projects` is unchanged.

## A live cross-peer divergence

The two peers publish incompatible types for the same conceptual Project
resource, both as documented contracts:

| Peer | Field | Type |
|---|---|---|
| cloud-api | `ProjectItem.id` | `string`, format `uuid` |
| `spelunk-server` | `Project.id` | `integer`, format `int64` |

This is live today, not a hypothetical. The CLI is unaffected for exactly one
reason: it never holds a typed project id. It carries the identifier as an
opaque string and spends it as a single percent-encoded path segment, so both
peers' shapes pass through untouched.

That immunity is load-bearing and invisible in the type signature, so it is
pinned by `project_id_stays_opaque_across_both_peers_id_types` in
`crates/spelunk-cli/src/server_client.rs`. Narrowing the project id to an `i64`
or a `Uuid` would make the CLI incompatible with one peer or the other; the
test exists to make that a loud failure rather than a discovery in production.

Reconciling the two peers is out of scope for this repository, which owns only
one of them.

The table itself is a **note, not a check**. cloud-api's schema lives in another
repository and is not vendored here, so nothing in this repo verifies those two
rows or notices when either one changes. Only the second row is checked, by the
`openapi-snapshot` job. Read the first as documentation of what was true when it
was transcribed. The test named above pins the CLI's immunity, which is what
actually protects you, and that holds whatever the two peers do next.

## Enforcement, and what it is worth

| Promise | Enforced by | Against what |
|---|---|---|
| Absent optionals degrade to documented defaults | `recorded_legacy_peers_degrade_to_documented_defaults` | Real recorded peer responses |
| Present optionals are actually read | `recorded_current_peers_parse_their_optional_objects` | Real recorded peer responses |
| Unknown fields and enum values are ignored | `unknown_fields_from_a_newer_peer_are_ignored` | Real recorded response, unknown fields added |
| No field can take the whole body down with it | `every_health_field_degrades_alone_rather_than_taking_the_body_down` | Every member of the recorded body, mutated one at a time |
| The project id stays opaque | `project_id_stays_opaque_across_both_peers_id_types` | Two peers' published id shapes |
| Memory read endpoints return an object envelope | `*_returns_object_envelope_not_bare_array` (spelunk-server `wire_shape_tests`) | The running handlers |
| The CLI reads both the envelope and a legacy bare array | `list_accepts_*` / `search_accepts_*` / `harvested_shas_accepts_both_shapes` (`storage/remote/tests.rs`), plus `cloud_first_reads_remotely_with_the_configured_slug_verbatim` end to end | Mock responses in both shapes; a real bare-array subprocess |
| Two real binaries complete the memory flow | `scripts/skew-smoke.sh`, run both ways by `.github/workflows/version-skew.yml` | Real released binaries |
| `/v1/` matches `docs/openapi.json` | `openapi-snapshot` job in `.github/workflows/ci.yml` | The running binary |

### Provenance of the fixtures

This matters more than it usually would. Almost every peer in this repository's
tests is a mock written to the shape we *believe* that peer has, which means
almost nothing here can falsify a premise about a real peer. Where a fixture is
real, that is worth knowing; where it is not, that is worth knowing more.

**Recorded from a running binary** (`crates/spelunk-cli/tests/fixtures/skew/`).
Released binaries where the version is released, the current build where it is
not, which is the distinction the last column exists to make:

| File | Source | Released binary |
|---|---|---|
| `health-v0.8.0.json` | `GET /v1/health` from the v0.8.0 `spelunk-server` | yes |
| `health-v0.9.0.json` | `GET /v1/health` from the v0.9.0 `spelunk-server` | yes |
| `health-v0.9.4-loading.json` | v0.9.4 `spelunk-server`, embedder still loading | yes |
| `health-v0.9.4-ready.json` | v0.9.4 `spelunk-server`, embedder ready | yes |
| `health-v0.9.5-loading.json` | current build, embedder still loading | no |
| `health-v0.9.5-ready.json` | current build, embedder ready | no |
| `openapi-v0.9.4.json` | `spelunk-server --print-openapi` from the v0.9.4 binary | yes |

The v0.8.0 and v0.9.0 bodies are the interesting ones: they genuinely omit
`embedder`, `embedding_dim`, and `limits`, so the absent-optional path is
exercised by a peer that really did behave that way rather than by a synthetic
body asserting our belief about one.

`openapi-v0.9.4.json` is a verbatim recording and is the one file in this
repository exempt from the house style rules on punctuation and issue
references. Editing it to conform would make it no longer a recording of what
that binary emits, which is the only property it has.

A recording is evidence only for as long as the live peer still sends the same
shape, and nothing was watching for that drift. `health-v0.9.5-ready.json` is
therefore also compared against a live body by a test in `spelunk-server`, so
the current server changing its health keys fails there rather than silently
turning a fixture into fiction. The v0.8.x and v0.9.0 recordings have no such
guard and cannot have one: those binaries are frozen, which is exactly why
their recordings stay useful.

**Hand-written to our belief:** the `health_body()` helper in
`crates/spelunk-cli/src/capability/probe.rs`, which predates this document, and
the unknown-field additions grafted onto the recorded v0.9.5 body (no peer
sends those fields yet, by construction: they stand in for a future one).

**Not represented at all:** cloud-api. Its schema lives in another repository
and is not vendored here, so no test in this repo validates anything against
it. The divergence table above is transcribed from its published schema, and
will go stale silently. Treat it as a note, not as a check.

### What these tests cannot tell you

The smoke test is the only part that puts two independently built artifacts on
a socket together, and it is therefore the only part that can contradict our
model of a peer rather than confirm it. Everything else, including the recorded
fixtures, is a replay: a recorded response proves what a peer *did* send once,
not what it will send under a different configuration, a different embedder
state, or a different deployment.

Two specific limits worth naming:

- The smoke test's search step depends on the server-side embedder having
  loaded a model, which is not a wire-contract property. It waits up to
  `SKEW_EMBEDDER_TIMEOUT_SECS` (wall clock, default 300) for the embedder to
  settle. An earlier draft that did not wait produced a convincing false
  positive: an old CLI appearing to fail against a new server, purely because
  that server was a debug build still warming up.

  If the embedder never settles, the run **fails** rather than quietly
  proceeding: search is the only step that drives the query-embedding path
  across the skew boundary, and a run that skipped it proved much less than it
  appears to have. Point `SKEW_MODEL_CACHE` at a directory that outlives the
  run (CI caches one) so the model download is paid once, or set
  `SKEW_ALLOW_SKIPPED_SEARCH=1` to accept the gap deliberately. A peer that
  publishes no `embedder` object at all, which is every release before v0.9.x,
  is detected directly instead of being waited on.
- The smoke test refuses to run two identical versions against each other,
  because a skew test that is not skewed passes while proving nothing.

And two gaps in what is covered at all, worth knowing before treating the table
above as a coverage claim:

- **Only the response direction is checked against schemas.** There are
  recorded peer responses replayed against CLI deserialization, but no fixtures
  validating what the CLI *sends* against a peer's schema. The recorded
  `openapi-v0.9.4.json` is committed and ready for exactly that, and nothing
  consumes it yet. The smoke test covers the request direction only in the
  sense that a real peer accepted the real calls it made.
- **Fixtures cannot catch a divergence both peers tolerate.** The project id
  above is the worked example: because it crosses the wire as an opaque string,
  every request and response fixture passes whether the peer calls it a `uuid`
  or an `int64`. A query parameter a peer accepts and then ignores is the same
  class of thing, and one such case is pinned in
  `crates/spelunk-core/src/storage/remote/tests.rs` rather than fixed. Where the
  two ends disagree without either one erroring, only a test that compares the
  two contracts directly will see it.

## What's next

- [Stability contract](stability.md): what each surface promises within a version
- [Server setup](server-setup.md): running a team server
- [Releasing](releasing.md): how a version is cut
