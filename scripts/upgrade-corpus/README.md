# Upgrade corpus (the "DB museum")

Artifacts written by real, released spelunk binaries, kept so every future build
can be tested against what users actually have on disk.

Every other migration test in this repo builds an old shape by hand. That tests
what we *believe* the old format was. This one tests what it *is*. The corpus is
cheap to capture while the releases are recent and impossible to reconstruct
faithfully later, which is the whole reason it exists.

## Layout

```
scripts/upgrade-corpus/
  generate.sh         rebuild the corpus from pinned releases
  capture_expect.py   read each artifact with plain SQL, write MANIFEST.json
  embed_stub.py       stand-in for the pre-1.0 embedding wire
  checksums.txt       pinned sha256 per release asset

crates/spelunk-cli/tests/
  fixtures/upgrade-corpus/
    MANIFEST.json     one entry per wing: producer, artifact, digest, expected content
    wings/<id>/       the artifact itself, gzipped
  upgrade_corpus.rs   opens every wing with the current build and asserts
```

## The wings

| wing | producer | what it pins |
| --- | --- | --- |
| `index-v0.9.2-pre-user-version` | 0.9.2 | the last release before `index.db` grew `PRAGMA user_version`, so its version has to be inferred from table shapes |
| `index-v0.8.3-float768` | 0.8.3 | the last release that wrote `FLOAT[768]` vectors, with vectors actually in the table |
| `memory-v0.9.3-pre-entity-id` | 0.9.3 | the last release before memory entries grew a content-addressed `entity_id` |
| `memory-v0.9.5` | 0.9.5 | entity-id era, with a supersede chain and a separately archived entry |
| `registry-v0.9.5` | 0.9.5 | two registered projects and a dependency link |
| `git-notes-eras` | 0.7.1 / 0.9.3 / 0.9.5 | all three note-writing eras on one `refs/notes/spelunk` |

The note eras were established by running the binaries, not by reading history:
releases up to and including **0.9.2 replace** a commit's note blob on every
add; the append-only JSON-lines log starts at **0.9.3** (still without
`entity_id`); entity-keying arrives at **0.9.5**. Because the older eras
replace, each era writes against its own commit, which is also what a
long-lived checkout looks like.

## What is real, and what is not

Real, and the entire point: the binaries are the published release assets,
pinned by the sha256 GitHub records for them. Every database file, its schema,
its `vec0` table declarations and every row in it were written by that binary.

Not real, and deliberately so: the **values** inside the embedding vectors.
Pre-1.0 releases embed by calling a `spelunk-server` that shipped an embedder
which no longer exists, and a current server answers on a wire shape those
binaries cannot parse, so neither can produce these wings. `embed_stub.py`
serves that era's `/v1/health` and embedding endpoints so the real old binary
can complete a real run.

`generate.sh` starts the stub for **five of the six wings**, not only the
768-dimension one. Where the stub runs and where synthetic values actually end
up on disk are two different lists, so both are spelled out:

| wing | stub runs during capture | synthetic vector values in the artifact |
| --- | --- | --- |
| `index-v0.8.3-float768` | yes, for `spelunk index` | yes, the chunk embeddings |
| `index-v0.9.2-pre-user-version` | yes, for `spelunk index` | yes, the chunk embeddings |
| `memory-v0.9.3-pre-entity-id` | yes, for `spelunk memory add` | yes, the note embeddings |
| `memory-v0.9.5` | yes, for `spelunk memory add` | yes, the note embeddings |
| `registry-v0.9.5` | yes: the capture indexes two repos in order to register and link them | none, `registry.db` stores no vectors |
| `git-notes-eras` | no, notes are written without embedding | none |

Note that the memory wings reach the stub through `memory add`, not through
`spelunk index`, so "every wing that runs `spelunk index`" is not the right
rule for which wings are affected. The list above is.

Vector values are irrelevant to a migration test: what is asserted is that the
right number of vectors survives, and that the dimension-upgrade path discards
768-dimension ones wholesale.

Nothing the stub says about itself reaches disk: no wing contains its instance
id, its address or its port, and no wing has an `index_meta` provenance row,
because that table post-dates all of them.

## Determinism

Vector values are derived from a hash of the chunk text, so they do not churn.
The artifacts as a whole are **not** byte-reproducible: the databases carry
wall-clock `indexed_at` / `created_at` / `registered_at` values, note ids are
epoch milliseconds, and the registry wing stores the absolute path of the
`mktemp` directory it was captured in. Regenerating a wing therefore produces a
different file even when nothing about the release changed. Do not regenerate a
wing you did not mean to change: `--only` exists for exactly this reason.

## Captured paths are foreign paths

A path stored inside a wing belongs to the machine that captured it, so it is a
**portability** constraint as well as a reproducibility one. The suite runs on
Windows as well as macOS and Linux, and a macOS path is not a valid path there:
`Path::is_absolute` is false for `/private/var/...` on Windows, which needs a
drive or UNC prefix, and `canonicalize`, `exists` and separator handling are
host-OS questions in the same way.

So assert a captured path by comparing it with what the artifact holds, read
out of the wing before the current build opens it, never by asking the host
whether it looks like a path. Equality is the same question on every runner and
is the stronger check anyway: a path rewritten to a different absolute path is
still mangled. Whole-component operations that only read the string, such as
`Path::starts_with`, are safe. This is what
`every_registry_wing_keeps_its_projects_and_dependency_links` does with the
`registry-v0.9.5` wing.

`checksums.txt` pins the release binaries, which says nothing about the
artifacts. The artifacts are pinned separately by the `sha256` recorded per
wing in `MANIFEST.json`, which the suite checks before asserting anything else.
A wing that is edited or regenerated without its expectations being recaptured
fails the suite rather than quietly asserting one artifact's contents against
another's.

## Regenerating

Needs `gh` (authenticated), `python3`, `sqlite3`, and `git`. No spelunk-server
and no model download.

```sh
scripts/upgrade-corpus/generate.sh              # every wing
scripts/upgrade-corpus/generate.sh --list
scripts/upgrade-corpus/generate.sh --only index-v0.9.2-pre-user-version
```

Wings are only rewritten when `--only` names them, so touching one does not
churn the others.

## Adding a wing at each release

1. Add the release asset's sha256 to `checksums.txt`. Take it from
   `gh api repos/<slug>/releases/tags/<tag>`, not from a local download alone.
2. Append `wing-id|tag|kind` to the `WINGS` table in `generate.sh`.
3. Run `generate.sh --only <wing-id>`. It captures the wing and then re-runs
   `capture_expect.py` over the whole corpus, which is what records the new
   wing's expectations and its `sha256` pin in `MANIFEST.json`. Untouched wings
   come back byte-identical, so the diff should be the new wing only.
4. Run `cargo test -p spelunk-cli --test upgrade_corpus`.
5. Check the old-binary leg against
   [When this contract flips](#when-this-contract-flips). A release carrying the
   `memory.db` version guard is expected to *refuse* a newer memory store rather
   than read it, and the criterion-4 assertion has to be updated to say so
   rather than relaxed.

The test is data-driven off `MANIFEST.json`, so a new wing of an existing kind
needs no Rust changes. A genuinely new *kind* of artifact needs a builder in
`generate.sh`, a reader in `capture_expect.py`, and an opener in the test. If it
stores paths, read [Captured paths are foreign paths](#captured-paths-are-foreign-paths)
before writing assertions about them.

## Size

The corpus checks in at well under 100 KB. A captured database is mostly the
`vec0` extension's preallocated vector chunk, which is zeros: 3.8 MB raw
compresses to 28 KB. Wings are stored gzipped and expanded into a temp dir by
the test, which it would do anyway, since opening a database migrates it and
would otherwise destroy the fixture on first run.

## CI

`.github/workflows/upgrade-corpus.yml` runs on `main`, release branches, version
tags, and any pull request touching the corpus, the suite, the storage layer or
the migrations. The main leg reads the checked-in fixtures only, so it cannot be
broken by a network hiccup. One extra step downloads a pinned release to check
what an old binary does with a database the current build has already upgraded.

## The old-binary contract, as measured

Measured against the **0.9.x releases**, and true of them rather than of any
old binary forever: a pinned release opening a current database does a **clean
read**, exit 0, correct counts, entries listed, full-text hits returned, no row
lost. See "When this contract flips" below before assuming it holds for a
release you are about to add.

One wrinkle the corpus surfaced: a release whose own schema version is
below the current one re-stamps `PRAGMA user_version` down to its own on close
(v0.9.3 rewinds an `index.db` from 15 to 14; v0.9.2 pre-dates the header and
never stamps; v0.9.5 stamps what it finds). That loses no data and self-heals,
because the steps above the rewound version are individually idempotent, so the
next current-build open re-runs them as no-ops and re-stamps the current
version. The test asserts that heal rather than assuming it.

The rewind is not a v0.9.3 quirk. It falls out of how the `index.db` runner
works in every build: it returns early only when the stamp already equals its
own `CURRENT_SCHEMA_VERSION`, and otherwise runs whatever steps are above the
value it read and stamps its own version at the end. A stamp *above* its own is
therefore written back down. Anyone debugging an `index.db` whose
`user_version` went backwards is looking at an older binary having opened it,
not at corruption, and the next open by a current build repairs it.

### When this contract flips

`memory.db` behaves differently from `index.db` here, and the difference is
deliberate. `MemoryStore::open` **refuses** a store whose `user_version` is
above the build's own `MEMORY_SCHEMA_VERSION`, with an upgrade message, rather
than opening it and rewinding it. Memory is not derived data and cannot be
rebuilt, so a refusal is the designed outcome. That is the promise recorded for
`.spelunk/memory.db` in [the stability contract](../../docs/stability.md#on-disk-formats).

Every 0.9.x release predates that guard, which is the only reason the clean-read
result above covers `memory.db` at all. The guard is in the current build and
unreleased. So the first wing captured from a release that carries it will,
once the current build's `MEMORY_SCHEMA_VERSION` moves past that release's,
make the old binary **refuse** the memory wing instead of listing it.

When that happens, `a_pinned_old_binary_reads_a_current_database_cleanly_and_loses_no_data`
starts failing on its `memory list` assertion. That failure is **correct** and
is the contract working. Do not weaken the assertion to make it pass: encode
the refusal instead, asserting a non-zero exit and the upgrade message, and
keep the `index.db` half of the test asserting the clean read and heal, which
is unaffected.
