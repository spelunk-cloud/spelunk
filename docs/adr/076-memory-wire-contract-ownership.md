# ADR-076: Ownership of the memory wire contract (CLI, team server, cloud-api)

**Date:** 2026-07-26
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** builds on [ADR-005](005-cli-slug-uuid-resolution.md)
(slug to UUID resolution, the mechanism at the center of one of this ADR's
sharpest findings), [ADR-059](059-git-notes-v1-format-freeze.md) D2 (the
`remote_id` optional-additive-field pattern this ADR generalizes into a
policy), and [ADR-062](062-temporal-as-of-semantic-search.md) (the temporal
fields whose absence on both server-side peers is documented here for the
first time). It does not reopen [ADR-056](056-oss-server-tenancy-model.md) or
[ADR-071](071-per-server-client-bearer-scoping.md); credential handling is
untouched. Every claim it makes about cloud-api is sourced to that service's
published OpenAPI contract, cited by operation id, schema name, route, verb
and field name. ADR numbering is a single sequence shared with cloud-api,
which is why this document is 076, not the next free number in this repo's
own listing.

## Context

### The founder's concern

The CLI's request/response structures for server memory
(`crates/spelunk-core/src/storage/remote/wire_types.rs` and
`crates/spelunk-core/src/storage/remote/sync.rs`) are hand-mirrored against
the team server's handler structs (`crates/spelunk-server/src/handlers.rs`),
and separately against cloud-api's published API contract.
Nothing enforces that the three agree. Johan: "I'm worried we have too
flexible a shared interface there": the concern being that
`#[serde(default)]` optionals and default unknown-field tolerance are load
bearing for compatibility today, and that same tolerance is what lets the
three implementations drift apart without anything failing loudly.

### Three peers, two independent wire clients, one config knob

A `server_url` in `cloud_first` mode is not restricted to the OSS team
server: `crates/spelunk-core/src/storage/mod.rs:104-119`'s
`open_remote_memory_backend` routes **any** configured `server_url` (team
server or cloud-api) through the same `RemoteMemoryBackend`
(`storage/remote/mod.rs`, built from `wire_types.rs`) for the full memory
CRUD surface (add/list/get/search/archive/supersede/list-by-source-ref).
Separately, `crates/spelunk-core/src/storage/remote/sync.rs`'s
`CloudSyncClient`, a second, independently hand-written wire client, is
used by `spelunk sync` / `spelunk memory push` for a different (batch +
delta) protocol, and per its own module doc, it also targets either peer
("the same client cloud-api's `/memory/batch` serves"). So there are two
separate CLI-side wire definitions, each hand-mirrored against two different
possible server implementations, none of the four pairings compiler-checked
against each other.

### This ADR's scope

Per the task that spawned it: inventory the duplicated surface, evaluate a
shared crate vs. schema-first vs.
status-quo-plus-contract-tests, assess which tolerances are deliberate
compatibility affordances versus accidental looseness, and reach a
YAGNI-justified recommendation. Wire-visible identity decisions (integer
rowid vs. UUID) are flagged, not resolved, here; they are their own
ADR-scoped decision. This ADR does not implement anything; it is a decision
record for a task in `refine`, headed to `implement` as this document and to
`verify` for Johan's sign-off.

A sibling piece of work (not cited by number below, referred to here as **the
version-skew contract-tests work**) is building recorded-fixture contract
tests and an n±1 team-server support policy in parallel. That work does not
block on this one, and this ADR's recommendation is written to compose with
it: whichever option below is chosen, "old peer without field X" still has to
be expressible, and the fixture harness that work builds is the natural place
to exercise it once this ADR's shared-definition or schema artifacts exist.

## Divergence inventory

Built by reading this repo's two implementations directly (`spelunk-core`'s
`storage/remote/wire_types.rs` and `storage/remote/sync.rs`, and
`spelunk-server`'s `handlers.rs`, `db.rs`, and its `migrations/*.sql`) and,
for cloud-api, by reading its published OpenAPI contract: the routes,
operations, schemas and field names that document declares.

### Table 1: CLI (spelunk-core) vs. team server (spelunk-server)

| Shape | CLI side | Team-server side | Divergence |
|---|---|---|---|
| Add-note request | `AddNoteRequest` (`wire_types.rs:8-19`): kind, title, body, tags, linked_files, embedding, `source_ref?`, `valid_at?` (both `skip_serializing_if`) | `AddNoteRequest` (`handlers.rs:194-211`): kind, title, body, tags, linked_files, embedding, **no `source_ref` or `valid_at` field at all** | Not an optionality gap: the server struct has no field to receive these, so serde silently drops them on deserialize (no `deny_unknown_fields` anywhere in the tree; verified by grep). `server_001.sql` through `server_007.sql` have no `source_ref`, `valid_at`, or `invalid_at` column either. A CLI push of a temporally-scoped or provenance-tagged memory entry through a team server **loses that data on write**, silently, every time. |
| Note read shape | `NoteResponse` (`wire_types.rs:42-64`): …, `source_ref?`, `valid_at?`, `invalid_at?`, `remote_id?`, `distance?` | `ServerNote` (`db.rs:75-96`): …, `remote_id?`, `distance?`, same missing three fields | Same gap on every read path (list/get/search): the team server cannot return what it never stored. |
| Batch push item | `BatchPushItem` (`sync.rs:71-94`): kind, title, body?, external_id, source_commit?, **`vector`?, `vector_model`?, `vector_precision`?** | `BatchNoteItem` (`handlers.rs:629-645`): kind, title, body?, external_id, source_commit?, **`embedding`?** (bare, no model/precision) | Field-**name** mismatch, not just optionality: the CLI's pushed-vector fast path (gated behind an `accepts_pushed_vectors` capability, `sync.rs:96-119`) sends `vector`/`vector_model`/`vector_precision`; the team server's struct only knows `embedding`. If the team server were ever made to advertise that capability, the pushed vector would land as three unrecognized field names and be silently dropped: the field the team server reads (`embedding`) is never the one the CLI would send in that mode. Currently latent only because nothing gates the team server into advertising the capability. |
| Delta pull entry | `RemoteEntry` (`sync.rs:151-162`): id, kind, title, body?, source_commit?, **`archived_at?`**, created_at; `is_archived()` reads it | `SinceIdEntry` (`handlers.rs:1143-1153`): id, kind, title, body?, source_commit?, created_at, **no `archived_at`** | Not an optional-field gap, a protocol gap: the team server's own doc comment (`handlers.rs:1176-1177`) states archived entries are excluded outright from the `since_id` cursor mode, rather than surfaced with a tombstone flag. A puller can learn "this stopped appearing" from the team server but never "this was archived", a strictly weaker signal than what `RemoteEntry::is_archived()` exists to consume from cloud-api. |
| `GET /v1/projects` (slug -> id resolution) | `CloudProjectItem { id: uuid::Uuid, slug? }` (`wire_types.rs:117-122`), consumed by `resolve_cloud_project_uuid` | `Project { id: i64, slug, embedding_dim, embedding_model?, created_at }` (`db.rs:68-77`), same route, same URL shape | **Live, reachable bug**, not a theoretical gap; detailed below. |
| Note identity | one field, `id: i64`, used everywhere it appears in a CLI-facing response | **three** distinct identities: `id` (i64 rowid; single-entry CRUD/archive/supersede), `remote_id` (opaque client-supplied string, the wire's `external_id`, batch-push idempotency key only), `sync_id` (server-minted UUIDv7, `db.rs:378`, used **only** for `/memory/since` ordering and returned as the JSON field literally named `id` in that one response, `handlers.rs:1140-1141,1207`) | The JSON field name `id` denotes a different type and a different value depending which endpoint's response it appears in. Well commented in-code, but nothing across the wire boundary enforces a caller cannot mix them up: a class of bug only a shared/generated type could catch at compile time. Flagged per this ADR's scope; not resolved here. |
| Search / bool / count | `SearchRequest`, `BoolResponse`, `CountResponse`, `SupersedeRequest` | Same names, same fields | Match. Listed for completeness: the inventory is not all divergence. |

**The `GET /v1/projects` bug, verified end to end:** `resolve_cloud_project_uuid`
(`storage/remote/mod.rs:138-164`) is guarded off only by
`crate::config::is_loopback_url(url)` (D6 in that function's own doc comment),
and it is not guarded by any check for "is this actually cloud-api." The
documented, canonical example for a **self-hosted** team server in
`docs/server-setup.md:377-378` is:

```toml
server_url = "https://spelunk.internal.example.com"
project_id = "my-awesome-app"
```

That is a non-loopback URL paired with a human slug, exactly the input pair that
skips the loopback guard and enters `resolve_cloud_project_uuid`, which calls
`GET /v1/projects` against that self-hosted team server and attempts to
deserialize `Project.id` (a JSON integer on the team server, confirmed above)
into `uuid::Uuid` (which requires a UUID-formatted JSON string). That
deserialize fails, and the whole memory-backend open fails with it. The team
server's routes are slug-keyed end to end (`Path<String>` throughout
`handlers.rs`, `db.upsert_project(&project_id, ...)` takes the slug directly).
It never needed UUID routing. `docs/server-setup.md:423-424`'s own phrasing
("If the server routes projects by an internal UUID, as a team/cloud memory
server does...") already blurs this distinction; the code inherits the same
blur, using loopback-ness as a proxy for "which peer is this" when the actual
distinguishing fact, whether this URL's `Project.id` is a UUID or an
integer, is not something either side declares, only something the client
guesses and cloud-api or self-hosted-ness happens to correlate with today.

### Table 2: CLI vs. cloud-api (evidence from cloud-api's published OpenAPI contract)

| Shape | CLI side | cloud-api side | Divergence |
|---|---|---|---|
| Route surface | `RemoteMemoryBackend` issues `POST .../memory/search`, `POST .../memory/{id}/archive`, `POST .../memory/{id}/supersede` unconditionally, for any `server_url` in `cloud_first` mode (`storage/remote/mod.rs:340-483`) | The contract declares **no** `/memory/search` route: search is `listMemoryEntries` (`GET .../memory`) with a `q` query parameter, unified with plain list and returning scored items (`SearchListResponse`). Archive is `deleteMemoryEntry` (`DELETE .../memory/{entry_id}`, 204), not a `POST .../archive`. There is **no supersede operation** at all; supersession is expressible only as a `BatchEdgeItem` with `kind: "supersedes"` inside `batchCreateMemoryEntries` | Three of `RemoteMemoryBackend`'s six CRUD methods target paths/verbs cloud-api does not publish. `cloud_first` mode against cloud-api is a real, documented configuration (it is the mechanism `resolve_cloud_project_uuid` exists for). This needs its own verification pass beyond what this ADR did (see the follow-up task filed alongside this document), but the route mismatch itself is read straight off the published contract, not inferred. |
| Temporal fields | `source_ref` / `valid_at` / `invalid_at` (ADR-062) | No `source_ref`, `valid_at` or `invalid_at` property on any schema in the contract (`CreateEntryBody`, `BatchEntryItem`, `EntryResponse`, `SearchEntryResponse`), and no operation that accepts one as a parameter | Same gap as the team server: cloud-api publishes no temporal-as-of vocabulary either. Two independent server surfaces, one shared absence, neither documented from the CLI's side. |
| Tags / linked files | `NoteResponse.tags: Vec<String>` and `.linked_files: Vec<String>` are **required** fields (no `#[serde(default)]`) | `CreateEntryBody` and `EntryResponse` declare no `tags` or `linked_files` property at all. The only `tags`/`files` in the contract are on `GraphNodeResponse`, from the unrelated project-graph operation, and both are documented there as deferred post-MVP and always empty | The omission is contract-declared, not merely unobserved: `EntryResponse` is the full published shape for both the create (201) and fetch (200) responses, and neither key appears in it. Deserializing such a response into the CLI's `NoteResponse` fails outright on a missing required field rather than defaulting. A concrete illustration of why "which fields are Option vs. required" is not a decision either side is making with the other side in view. |
| Embedding field name | `vector` / `vector_model` / `vector_precision` (`sync.rs`, matches cloud-api, see below) vs. `embedding` (`wire_types.rs`'s `AddNoteRequest`, used for the non-batch add path) | `CreateEntryBody.vector` / `.vector_model` / `.vector_precision`, and the same three on `BatchEntryItem` | `sync.rs`'s naming already matches cloud-api's (evidence the two were designed against each other at least once), while `wire_types.rs`'s plain `AddNoteRequest.embedding` (the single-note add path) does not match either peer's batch naming. Three names for what is conceptually one field across the three surfaces in play. |
| Identity model | (as above) | **Single** UUID. `EntryResponse.id` (`format: uuid`) is also the `entry_id` path parameter of `getMemoryEntry` and `deleteMemoryEntry`, and `listMemorySince`'s preferred cursor is a UUIDv7 `since_id` over that same id. `external_id` is a separate, client-supplied idempotency key: `batchCreateMemoryEntries` is documented as idempotent on it, and `BatchEdgeItem` addresses entries by `from_external_id` / `to_external_id`. That makes `external_id` the equivalent of the team server's `remote_id` | cloud-api's identity model is simpler than the team server's three-way split (one UUID does what the team server splits across `id`/`sync_id`), but is a **third, different** shape from either: nothing shared between the two server-side implementations' identity models, and the CLI has to already know which shape it is talking to for calls that use an id as a path parameter. |
| Extra cloud-api-only fields | none of these exist client-side | `entry_type`, `embedding_dim`, `author` (`AuthorInfo`), and `superseded` / `superseded_by`, all declared on `EntryResponse` | Not necessarily a problem (the CLI can ignore fields it doesn't need on responses it already parses tolerantly) but widens the actual shape gap between "the CLI's idea of a note" and what cloud-api's contract declares, well past what `wire_types.rs`'s comments suggest. |

### The reconciliation pattern already has a track record

A prior reconciliation between these same surfaces is legible in the
published contract itself: `getHealth`'s `HealthResponse` declares an explicit
capability vocabulary, `listMemorySince` documents a UUIDv7 `since_id` cursor
as the preferred, drift-free form alongside the timestamp form kept for SSE
catch-up, and the `project_id` path parameter is documented on every memory
operation as accepting either a UUID or a slug. None of those points could be
derived from one surface alone; each had to be agreed across them.

What that same contract still has no vocabulary for is
`source_ref`/`valid_at`/`invalid_at`, or `tags`/`linked_files`. Those gaps
were not judged acceptable and left out; they were not in view. Ad hoc,
find-it-when-someone-notices reconciliation has already happened once and
already missed a category of drift this review found by comparing the CLI's
structs against each peer's declared surface field by field, rather than by
symptom.

## Options considered

### a. Shared `spelunk-wire` crate

Serde structs (and any request validation worth centralizing) owned once,
consumed as a workspace member by `spelunk-core` and `spelunk-server`, and by
cloud-api if practical.

**Drift prevention:** total, by construction, for whichever consumers depend
on it: two callers cannot disagree about a struct's field names or
optionality if they compile against the same type. This is the only option
of the three that converts today's field-name mismatches (`vector` vs.
`embedding`) and missing-field gaps (`source_ref`/`valid_at`) into compile
errors instead of silent drops, for the leg(s) it covers.

**Skew-window support:** unaffected, by design: a shared struct still needs
`Option<T>` and `#[serde(default)]` fields to express "a peer at version n-1
doesn't have field X yet." Sharing the struct does not remove the need for
that vocabulary; it removes the possibility of the *two sides* disagreeing
about which fields carry that vocabulary, which is the actual defect found
above (the team server not merely tolerating an absent `source_ref` but
having no `source_ref` concept to tolerate).

**Build/release coupling cost, split by leg:**

- **CLI <-> team server:** verified to be already near zero. `spelunk-core`,
  `spelunk-cli`, `spelunk-server`, and `spelunk-embed` are four members of one
  Cargo workspace (root `Cargo.toml:1-7`), version-locked today at `0.9.5` in
  every crate's own `Cargo.toml`, and `.github/workflows/release.yml` builds
  and ships both binaries from one `v*.*.*` tag in one job matrix. These two
  are *already* released in lockstep, from the same repo, at the same
  version. Adding a `spelunk-wire` workspace member costs no *incremental*
  build/release coupling here: the coupling this option is usually charged
  for already fully exists. (A deployed team-server instance running an old
  binary against a newer CLI is a *runtime* compatibility question, which is
  what the version-skew contract-tests work's n±1 policy is for. That is
  orthogonal to *build-time* source coupling, which this workspace already
  has completely.)
- **CLI <-> cloud-api:** the opposite. cloud-api is a separate service,
  released independently of spelunk-oss's version tags. Pulling it into the
  same compiled crate means either vendoring it as a path dependency
  (impossible, separate repos) or a git/registry dependency pinned to a
  spelunk-oss commit or tag. Either way, a wire-contract change could not
  reach cloud-api's published contract until spelunk-oss cut a release, or
  cloud-api's pin would have to track unreleased commits, trading the coupling
  cost for a stability cost. Today that leg pays neither: cloud-api publishes
  its contract independently of spelunk-oss's release tags, and the CLI
  consumes it.

### b. Schema-first (OpenAPI/JSON Schema as the source of truth, generated per consumer)

**Drift prevention:** strong, and asymmetric with what already exists.
cloud-api already practises it for its own contract: it publishes an OpenAPI
document and treats that document as authoritative for its HTTP surface. The
team server already has the *server-side half* of this exact pattern:
`utoipa`-derived `ApiDoc` (`spelunk-server/src/lib.rs:425-426,505`), served
live at `/api-docs/openapi.json`, snapshotted to the checked-in
`docs/openapi.json` (`lib.rs:948-966`), and CI-diffed against a freshly
generated copy (`.github/workflows/ci.yml:254-257`), so the team server's
own handler structs cannot drift from *their own* published spec today. What
is missing on both legs is the other half: nothing validates
`spelunk-core`'s independently hand-written `wire_types.rs`/`sync.rs`
against either spec. This option is roughly eighty percent already built for
the team-server leg and entirely available as prior art for the cloud-api
leg; the gap is a generation/validation step on the CLI side, not new
server-side infrastructure.

**Skew-window support:** natural fit. A vendored schema snapshot per
supported peer version is exactly the shape the version-skew contract-tests
work needs for its recorded-fixture validation, whichever peer version the
fixture targets.

**Build/release coupling cost:** low on both legs, and, unlike option (a),
*not* asymmetric between them. Nothing is compiled together across repos; a
consumer regenerates types from a checked-in spec snapshot at its own pace
and catches drift at build/CI time (a failing contract test), not at deploy
time (a blocked release). cloud-api's own release process is untouched; the
team-server leg keeps the CI-enforced generation it already
has; the CLI adds a codegen or contract-test step against whichever spec
snapshot it vendors, on its own release cadence, independent of either
server's cadence.

### c. Status quo + contract tests only

Tolerate the duplication; catch drift mechanically via the fixtures the
version-skew contract-tests work is already building.

**Drift prevention:** partial, and retrospective by construction. Contract
tests validate *today's* behavior; they cannot invent the assertion "the team
server should accept `source_ref`" unless someone already knows to write it.
The prior reconciliation described above is direct evidence of exactly this
failure mode: a real pass, done carefully, still left the temporal-field and
tags/linked_files gaps because nobody was looking at the right diff. A
contract-test suite built without a shared definition or a schema to check
against would need to independently rediscover every gap this review found
by comparing declared surfaces field by field, and would canonize any gap it
doesn't happen to think to test as "expected", rather than closing it.

**Skew-window support:** fine: this is what the fixtures are for regardless
of which other option is chosen.

**Build/release coupling cost:** none, which is the option's only real
advantage.

## Decision

> **Amendment: the `GET /v1/projects` divergence is no longer reachable from
> the CLI, and Table 2's route-surface row is sharpened.** The inventory above
> records what the three implementations looked like on 2026-07-26 and is left
> as provenance. Both findings were accurate as written; this records what has
> changed under them.
>
> **Table 1's `GET /v1/projects` row, and the prose under it.** The finding was
> correct: this was a live, reachable bug, and the documented `cloud_first`
> client configuration for a self-hosted team server hit it on every memory
> command. The slug to UUID resolver named there, `resolve_cloud_project_uuid`,
> has now been deleted, along with its `.spelunk/cloud-project-id.lock` cache
> and its `CloudProjectItem` / `CloudProjectListResponse` wire types.
> `Config.project_id` is passed verbatim as the project path segment, so the
> memory-backend open path issues no `GET /v1/projects` request at all and the
> response-shape divergence between the two peers is unreachable from the CLI.
> That deletion, not a reconciliation of the two shapes, is what retires the
> row. See the amendment on the Decision section of
> [ADR-005](005-cli-slug-uuid-resolution.md).
>
> **Table 2's route-surface row stands, and the verification pass it asks for
> has been done.** It undercounts. The mismatch is not three of
> `RemoteMemoryBackend`'s six CRUD methods but all six, because the divergence
> is an identity mismatch before it is a routing one: cloud-api keys memory
> entries by a UUIDv7 while `RemoteMemoryBackend`'s wire vocabulary is `i64`
> end to end, so `add` and `list` fail on the entry id itself before any route
> lookup is reached. The row's conclusion, that these are two protocols rather
> than a set of route typos a shim could close, survives the pass and is
> strengthened by it.
>
> Making `cloud_first` work against the hosted API is therefore a change to the
> memory identity model rather than a route-surface patch, and it is tracked
> separately from this ADR. Nothing in it is retracted here.
>
> Live surface: `open_remote_memory_backend_with_bearer` in
> `crates/spelunk-core/src/storage/mod.rs`.

**Split the contract by leg, matching each to the tool whose release model
already fits it, and keep contract tests under both:**

1. **CLI <-> team server: extract a shared `spelunk-wire` crate** as a new
   workspace member, owning the serde structs (and field-presence semantics)
   currently duplicated between `spelunk-core/storage/remote/` and
   `spelunk-server/handlers.rs`. This directly eliminates, by construction,
   the temporal-field silent-drop and the `vector`/`embedding` field-name
   mismatch found in Table 1, the two current defects on this leg that are
   not skew-tolerance at all, just drift. Justified because the build/release
   coupling this option is normally charged for is verified above to already
   be fully paid: this repo already ships both binaries from one workspace at
   one version on one tag.

2. **CLI <-> cloud-api: schema-first**, building on infrastructure that
   already exists on both ends of this leg rather than inventing a third.
   cloud-api's published OpenAPI document becomes the
   contract source for this leg; `spelunk-core`'s cloud-facing wire code
   (`sync.rs`'s `CloudSyncClient`, and `RemoteMemoryBackend` when
   `server_url` resolves to cloud-api) is validated against a vendored
   snapshot of that spec via a contract test, in the same harness the
   version-skew contract-tests work is already building. Justified because
   option (a) applied to this leg costs real cross-repo release coupling
   (above), while schema-first costs comparably little and reuses machinery
   that already exists on both sides.

3. **Do not force one wire definition across all three.** cloud-api's route
   surface has already diverged from the team server's by more than
   optionality: no `/memory/search`, `DELETE` instead of `POST .../archive`,
   no `/memory/{id}/supersede`, no `tags`/`linked_files` at all (Table 2). A
   single shared shape spanning all three today would have to invent a
   lowest-common-denominator API matching neither deployed surface, which is
   larger surgery than the flexibility question asked and not justified by
   this inventory. Treat CLI<->team-server and CLI<->cloud-api as two
   contracts, each strengthened by the mechanism that fits its actual release
   model, both feeding the same contract-test harness.

4. **Keep the version-skew contract-tests work unblocked and, once either
   artifact above lands, point its fixtures at it.** That work's fixtures are
   valuable under any outcome (per its own framing) and should proceed in
   parallel; this ADR only changes what the fixtures assert against once the
   shared crate and the vendored schema exist.

### Which flexibility is deliberate, which is accidental

The founder's question was not only "should we own this once" but "which of
the tolerances we already have are doing real work." Sorted from what this
review found:

- **Deliberate, keep it:** `remote_id: Option<String>` with
  `#[serde(default)]` (ADR-059 D2) is a documented, additive compatibility
  affordance: an old peer without the field reads as `None`, by design, and
  that is exactly the vocabulary a skew policy needs. `source_ref` /
  `valid_at` / `invalid_at`'s optionality on the CLI's own local `Note` type
  is the same pattern, added post hoc for ADR-062, and should stay optional
  in whatever shared definition replaces `wire_types.rs`'s copy: the fix is
  giving the team server a field to be absent *from* the peer that has it,
  not making it universally required.
- **Accidental, not a skew affordance at all:** the team server's
  `AddNoteRequest` / `ServerNote` having **no field** for `source_ref` /
  `valid_at` / `invalid_at` is not tolerance, it is absence: there is
  nothing here for `#[serde(default)]` to be doing compatibility work for,
  because the concept was never wired to that peer. Same for the
  `vector`/`embedding` field-name mismatch and the `GET /v1/projects`
  `Uuid`/`i64` mismatch: neither is a version boundary, both are two
  structs that were never checked against each other.
- **On `deny_unknown_fields`:** appropriate only on the *receiving* side of a
  request, and only where an unrecognized field indicates a caller/server
  naming mismatch worth failing loudly on (this is precisely what would have
  caught `vector` vs. `embedding` immediately, at the first request, instead
  of via a structural read of both codebases). **Not** appropriate on the
  CLI's own *response*-deserialization types: those must keep tolerating
  unknown fields from a newer peer, which is the other half of the n±1 skew
  policy the version-skew contract-tests work is defining, and tightening
  that half would break forward compatibility the CLI depends on today.

## Non-goals

- **Not resolving the identity model.** Table 1's three-way `id`/`remote_id`/
  `sync_id` split on the team server, and its contrast with cloud-api's
  single UUID, is flagged per this ADR's scope and left as its own
  ADR-scoped decision: this document does not choose rowid vs. UUID vs.
  something else as the canonical wire identity.
- **Not implementing the crate or the schema tooling.** This is a decision
  record; the extraction, the codegen choice for consuming `openapi.yaml`
  from Rust, and the contract-test wiring are implementation tasks that
  follow from this decision, not part of it.
- **Not re-litigating cloud-api's route surface.** Whether cloud-api should
  grow `/memory/search`, a real supersede route, or `tags`/`linked_files`
  support is a cloud-api-side product/architecture question this document
  surfaces as evidence, not one it answers.
- **Not blocking the version-skew contract-tests work.** That work's fixture
  harness and n±1 policy proceed independently; this ADR only changes what
  the fixtures eventually validate against.

## Consequences

- **Two known-live defects get a fix path, not just a write-up.** The
  temporal-field silent drop and the `vector`/`embedding` mismatch on the
  CLI<->team-server leg are eliminated by construction once the shared crate
  lands, rather than requiring another discovered-by-hand reconciliation pass.
- **The `GET /v1/projects` bug and the cloud-api route-surface mismatches
  found in this review are not fixed by this ADR.** They are implementation
  bugs, not architecture questions, and are being filed as a separate,
  regular engineering task rather than folded into this decision record.
- **spelunk-core gains a new internal dependency** (`spelunk-wire`), which is
  a routine workspace restructuring cost, not a release-cadence cost, per the
  coupling analysis above.
- **The CLI's cloud-facing wire code gains a build/CI-time dependency on a
  vendored cloud-api schema snapshot.** Keeping that snapshot current is an
  ongoing maintenance cost, materially cheaper than the status quo's
  find-it-by-reading-both-codebases cost, but not zero: it is a
  vendored-file update, most naturally landing whenever cloud-api's published
  contract gains a field the CLI needs to consume.
- **The status quo's duplication does not disappear entirely.** cloud-api's
  own request/response types stay cloud-api's concern (that is option (b) as
  already practised on that side); this ADR does not ask cloud-api to consume
  a spelunk-oss-owned schema, only to keep publishing the contract it already
  treats as authoritative.

## Security implications

No new trust boundary and no attack-surface change: both proposed mechanisms
(a shared crate, a vendored schema) operate entirely on request/response
*shape*, not on authentication, authorization, or transport. The reliability
consequence is the material one: the temporal-field silent-drop and the
route-surface mismatches found here are data-integrity and availability
concerns (memory entries losing fields, or specific commands failing against
a real deployed peer) rather than security defects, and are treated as such,
filed as an engineering follow-up rather than escalated as a security
finding.

## Trigger conditions to revisit

- **cloud-api's route surface converges toward the team server's** (a
  `search`/`supersede`/`archive`-parity pass ships on cloud-api). At that
  point the "two separate contracts" split in the Decision should be
  revisited: a single schema-first source might then reasonably cover both
  legs.
- **cloud-api and spelunk-oss come to share a release cadence**, such that a
  shared compiled crate could be version-locked across both. The coupling-cost
  argument against folding cloud-api into that crate rests entirely on the two
  being released independently; if that stops being true, option (a) could
  reasonably extend to cover cloud-api too.
- **The shared `spelunk-wire` crate starts accumulating `Option<T>` /
  `#[serde(default)]` fields for reasons other than genuine cross-version
  compatibility.** That would mean the crate relocated the discipline problem
  instead of fixing it, and is the signal to revisit this decision rather
  than add another tolerant field to the shared type.
