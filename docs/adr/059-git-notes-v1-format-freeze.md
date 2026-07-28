# ADR-059: git-notes v1 format freeze (JSONL canonical + optional remote_id)

**Date:** 2026-07-06
**Deciders:** founder (Johan), architect

## Context

The distributed on-disk surface that spelunk writes to a repository is the
`refs/notes/spelunk` git-notes ref. Because git notes travel with the repo, this
blob format becomes a compatibility contract the moment v1.0 is tagged: any
reader in the wild must keep parsing blobs written by any prior or future
writer. Two independent defects in that surface must be resolved before the tag,
and both touch the same `NoteRecord` / `refs/notes/spelunk` shape, so they are
frozen together here.

### P0.1 – two incompatible writers for `refs/notes/spelunk`

There are two writers, and they disagree on the blob format:

- The default write-through path (`store_in_git_notes = true`) appends via
  `append_to_git_notes` (`crates/spelunk-core/src/storage/git_notes/mod.rs:29`),
  which does read-modify-append and produces a **JSON Lines** blob (one
  `NoteRecord` per line).
- `GitNotesBackend` (reached only by `--backend git-notes`) treats each blob as a
  **single JSON object**:
  - `read_record` (`mod.rs:288`) calls `serde_json::from_str` on the whole blob
    and returns `Err` on failure. A blob with more than one JSON line fails with
    "trailing characters"; `collect` (`mod.rs:313`) propagates that error via
    `?`, so a single multi-entry commit poisons the entire
    `memory list --backend git` output.
  - `add` (`backend_impl.rs:12`) and `archive` (`backend_impl.rs:83`) serialize a
    single record and write it with `git notes add -f` over stdin
    (`add_note_stdin`, `mod.rs:206`). Force-replacing the note with one record
    **silently deletes every sibling entry and every foreign line** on that
    commit's note.

The write-through helper already skips malformed lines when appending, but the
`GitNotesBackend` read and write paths do not, which is the inconsistency.

### P0.2 – stable identity never reached the distributed formats

Migration 020 (`crates/spelunk-core/migrations/020_memory_uuid.sql`) added
`uuid` and `remote_id` columns to the local `notes` table because the local `id`
is an autoincrement i64 that is not stable across machines. That identity never
reached the surfaces that leave the local machine:

- `NoteRecord` (`crates/spelunk-core/src/storage/note_record.rs`) serializes the
  local i64 `id` into git-notes with no uuid.
- The OSS server wire types (`crates/spelunk-core/src/storage/remote/wire_types.rs`)
  and the server `notes` table (`crates/spelunk-server/migrations/server_001.sql`)
  expose a per-server autoincrement rowid.

So the same logical memory entry carries a different, machine-local integer on
every machine and every server, with nothing to correlate them.

### Read-wiring finding (bounds the P0.2 change)

The founder ruling asked for confirmation of whether git-notes are currently
write-only. Confirmed by reading the code:

- The only reader of `refs/notes/spelunk` is `GitNotesBackend`
  (`read_record` / `collect` / `list` / `get`).
- `GitNotesBackend` is constructed in exactly one place: `open_memory_backend`
  (`crates/spelunk-core/src/storage/mod.rs:92`) returns it only when the caller
  passes `backend_override = Some("git-notes")`, i.e. the explicit
  `--backend git-notes` flag on a `memory` command.
- The default path writes notes (`append_to_git_notes` from
  `crates/spelunk-cli/src/cli/cmd/memory/add.rs:118`) but nothing reads them back
  automatically. `spelunk memory reconcile` imports from the local daemon's
  `server.db`, not from git-notes.

Conclusion: there is a manual read path (the `--backend git-notes` inspection
command), but **no automatic sync or reconcile consumer of git-notes exists**.
The identity change in this ADR is therefore a pure additive serialization
change; it does not need to build any consumer.

## Decision

Freeze the v1 git-notes surface with two changes. The `NoteRecord` shape, the
JSONL blob format, and the optional `remote_id` field are the frozen v1 contract.

### D1 – canonical format is JSONL; permissive read, strict write

We do not own the `refs/notes/spelunk` store; other tools and humans may write
prose or their own content into the same note. The canonical spelunk format is
**JSON Lines** (one `NoteRecord` per line), but we read permissively and write
without clobbering anything we did not author.

**Permissive read (kills `?`-poisoning).** Reading a note blob is line-oriented:

1. Split the blob on `\n`.
2. For each line, a line is a **spelunk record** iff it parses as a JSON *object*
   (`serde_json::from_str::<NoteRecord>`) with the expected `NoteRecord` shape.
   Any line that is not valid JSON, is valid JSON but not an object (array,
   string, number, `null`), or is an object that does not deserialize into
   `NoteRecord`, is a **foreign line**.
3. Foreign lines are **skipped on read** with no error. Blank lines are foreign.
4. A parse failure on one line never fails the read of the blob. One bad line
   never fails the whole `list`. The `schema_version > 1` guard is retained and
   is the *only* condition that returns an error from the reader (a record from a
   newer, incompatible schema is a real incompatibility, not foreign content).
   `schema_version` keeps its existing `#[serde(default)]` so a record with the
   field absent is version 0 and still reads.

A note that contains several spelunk records yields several `NoteRecord`s (the
current `read_record` returning `Option<NoteRecord>` becomes a multi-record read;
`collect` / `list` / `get` iterate the records within each commit's note as well
as across commits).

**Strict, preserving write (never clobber siblings or foreign content).**
`add` and `archive` must stop force-replacing the whole note. Both route through
a single read-modify-write helper that operates on the note as an ordered list of
lines:

- **Read** the current note blob for the target object (empty if none).
- **Classify** every line as a spelunk record or a foreign line, *preserving
  original order and the exact original text of every line*.
- **Apply** the mutation to spelunk records only:
  - `add`: append the new record as a new JSON line at the end of the blob.
  - `archive`: rewrite in place the single spelunk record whose `id` matches
    (set `status = "archived"`); all other lines, spelunk or foreign, are
    emitted unchanged in their original positions.
- **Write** the reassembled blob back with the existing stdin-based
  `git notes add -f -F - -- <object>` invocation (which keeps note content off
  argv). `-f` is still correct here: we are replacing the note with a blob that
  contains all prior content plus our targeted edit.

Foreign lines are never parsed, never reordered, and never dropped. Sibling
spelunk records that are not the mutation target are emitted byte-for-byte as
read. Serialization of *our* records uses the canonical `serde_json::to_string`
one-line form; a record we did not touch is re-emitted from its original source
text, not re-serialized (so we never reformat another writer's spacing).

> **Clarification (2026-07-12, collision surface is narrower than the opening
> sentence implies):** The "we do not own the store; other tools and humans may
> write into the same note" framing above overstates the collision risk.
> `refs/notes/spelunk` is a spelunk-specific notes ref, not git's default
> `refs/notes/commits`, so no default git tooling writes to it. The realistic
> foreign-content surface is limited to another tool deliberately targeting this
> custom ref, or a human editing it by hand. The permissive-read,
> non-clobbering-write design still stands regardless: it is sound defensive
> engineering independent of how narrow that surface is, and it is what lets the
> interleaved-prose conformance fixture below round-trip.

### D2 – optional additive `remote_id` (uuid)

Add an **optional, additive** `remote_id` to the three distributed surfaces. It
is the canonical cross-machine identity, set when an entry is synced to a remote
server. The local i64 `id` remains the in-process key and is what lets a user
create and work offline with or without a remote.

- **`NoteRecord`** (`note_record.rs`): add
  `#[serde(skip_serializing_if = "Option::is_none", default)] pub remote_id: Option<String>`.
  The value is a uuid string when present. Absent on the wire means `None`.
  `#[serde(default)]` means an old blob with no `remote_id` reads as `None`; a
  new writer omits the key entirely when it is `None`, so an old reader that does
  not know the field simply ignores it. The value is populated from the local
  `notes.remote_id` column (migration 020) when a record is serialized for a row
  that has been synced; write-through for a never-synced local row emits `None`.
- **Server `notes` table** (`crates/spelunk-server/migrations/server_001.sql`,
  applied as a new forward migration, not an edit to 001): add a nullable
  `remote_id TEXT` column with a partial `UNIQUE` index
  (`WHERE remote_id IS NOT NULL`), mirroring the client shape in migration 020.
  Existing rows keep `NULL`.
- **`/v1` wire responses** (`wire_types.rs` `NoteResponse`, and the matching
  server response body): add
  `#[serde(default)] pub remote_id: Option<String>`. `#[serde(default)]` makes an
  older server's response (no field) deserialize as `None`. `AddNoteResponse`
  gains the same optional field so a client can record the server-assigned
  `remote_id` on write.

Backward-compatibility rules, uniform across all three surfaces:

- The field is always optional. Absent == `None`. Never required.
- Old readers ignore an unknown `remote_id`; new readers treat its absence as
  `None`. No reader errors on its presence or absence.
- No existing field changes type, name, or nullability. The local i64 `id` is
  untouched and remains the primary in-process identity.

## Non-goals

- **git-notes as a sync mechanism is out of scope.** This ADR does not build any
  automatic consumer that reads `refs/notes/spelunk` to reconcile or converge
  memory across machines. That is a planned post-v1 feature and is too large to
  add at this stage. The read-wiring finding above establishes that no such
  consumer exists today; D1 only fixes the existing manual `--backend git-notes`
  read/write and the write-through append, and D2 only adds a field.
- **No new identity minting policy.** How and when `remote_id` is assigned on
  sync is already defined by migration 020 and the project's sync/reconciliation design, and is unchanged. This ADR
  only carries the existing column onto the distributed formats.
- **No semantic-search or graph capability** is added to `GitNotesBackend`; its
  unsupported methods stay unsupported.

## Conformance fixture

The following note blob (interleaving markdown prose with spelunk records) MUST
round-trip: reading it yields exactly the three `decision` records below in
order, ignoring the prose; and a subsequent `add` or `archive` MUST retain every
prose line and every untargeted record byte-for-byte in its original position.

```
# Implement payment by Stripe

We're implementing a payment rail, in this case Stripe...

{ "kind": "decision", "memory": "use stripe for payment processing" }

## Technical details

...Axum as the http handler layer.

{ "kind": "decision", "memory": "rust is our language of choice" }
{ "kind": "decision", "memory": "use axum to implement api's over restful http" }
```

Conformance requirements an implementation must satisfy:

1. **Read** of this blob returns three records and no error. The four prose
   blocks and the blank lines are skipped, not surfaced and not errored on. (The
   `{ "kind": ..., "memory": ... }` lines above are the founder's illustration of
   an interleaved record line; the real serialized shape is a full `NoteRecord`.
   The test fixture should use real `NoteRecord` JSON lines interleaved with the
   same prose.)
2. **`add`** of a new record appends one JSON line at the end; re-reading the
   blob still returns the original prose unchanged and now four records.
3. **`archive`** of the middle record sets only that record's `status` to
   `archived`; the other two records and all prose lines are unchanged in
   content and position.
4. A blob containing a single foreign line and no spelunk records reads as an
   empty record list with no error.

## Consequences

- **Easier:** one multi-entry commit no longer poisons `memory list`; archiving
  one entry no longer deletes its siblings or a co-located tool's data; the same
  logical entry can be correlated across machines by `remote_id` once synced.
- **Frozen:** after the v1.0 tag, the JSONL blob format, the `NoteRecord` field
  set (including optional `remote_id`), and the optional-field backward-compat
  rules are a contract. A future incompatible change bumps `schema_version` and
  writes a new ADR.
- **Revisit if:** a real notes-as-sync consumer is built (post-v1), which will
  need its own ADR and will build on this frozen format.

## Security implications

- The preserving read-modify-write must not let foreign content change the
  meaning of our records: foreign lines are opaque bytes that are copied through,
  never parsed and never executed. Note bodies (which may contain arbitrary user
  or LLM text) continue to be passed to git over stdin via `-F -`, never on
  argv, and `--` continues to guard the trailing object argument.
- `remote_id` is an opaque identity string; it carries no authority on its own.
  Read/write authorization on a shared server is unchanged and remains governed
  by [ADR-056](056-oss-server-tenancy-model.md) (single trust domain, shared
  key). Adding the column does not introduce a new trust boundary.
- Existing secret-scanning on the write path is unaffected: this ADR changes how
  a note blob is assembled, not what is scanned before a record is created.
