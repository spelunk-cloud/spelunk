# Plumbing Commands — Protocol Specification

spelunk follows the git model: **plumbing** commands are low-level primitives
designed for agents and scripts, while **porcelain** commands are
human-readable wrappers built on top of them.

---

## Design Principles

1. **JSONL on stdout** — every plumbing command writes one JSON object per
   line to stdout. Consumers `jq`-filter or stream-parse without buffering.
2. **Exit codes are semantic**
   - `0` — success, ≥1 result returned
   - `1` — success, but no results (empty set — not an error)
   - `2` — error (message on stderr, nothing on stdout)
3. **Errors to stderr only** — stdout is a clean data stream. Warnings and
   progress notes may go to stderr; they never appear on stdout.
4. **Composable** — plumbing commands read from stdin or file-path arguments;
   they do not open editors, prompt for input, or write to the DB unless
   that is their explicit job.
5. **`--format` is absent** — plumbing commands always emit JSONL; the flag
   is a porcelain concern.

---

## Plumbing Commands Reference

### `spelunk plumbing cat-chunks <file-path>`

Emit each indexed chunk for a file as a JSON object.

**Output fields per line:**

```json
{
  "id": 42,
  "kind": "function",
  "name": "do_thing",
  "start_line": 10,
  "end_line": 35,
  "content": "fn do_thing() { … }",
  "token_count": 120,
  "language": "rust"
}
```

Exit `1` if the file is not in the index.

---

### `spelunk plumbing ls-files [--prefix <path>]`

List every file tracked in the current project index.

**Output fields per line:**

```json
{
  "path": "src/main.rs",
  "language": "rust",
  "chunk_count": 14,
  "indexed_at": 1712345678
}
```

`--prefix` filters to files whose path starts with the given string.

---

### `spelunk plumbing parse-file <file-path>`

Parse a file on disk and emit chunks **without storing anything** in the DB.
Useful for previewing what the indexer would produce.

**Output fields per line:** same schema as `cat-chunks`.

Exit `2` if the file type is unsupported or the file cannot be read.

---

### `spelunk plumbing hash-file <file-path>`

Emit the blake3 hash for a single file and whether the index is current.

```json
{
  "path": "src/main.rs",
  "hash": "a3f7…",
  "indexed_hash": "a3f7…",
  "is_current": true
}
```

`indexed_hash` is `null` and `is_current` is `false` when the file is not
in the index.

---

### `spelunk plumbing knn <query>`

Embed `query` and return the top-K nearest chunks.

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--limit <n>` | 10 | Maximum results |
| `--min-score <f>` | 0.0 | Drop results below this cosine similarity |
| `--lang <lang>` | — | Restrict to chunks from files of this language |

**Output fields per line:**

```json
{
  "chunk_id": 42,
  "file": "src/main.rs",
  "name": "do_thing",
  "kind": "function",
  "start_line": 10,
  "end_line": 35,
  "score": 0.923,
  "content": "fn do_thing() { … }"
}
```

Exit `1` if no chunks are above `--min-score`.

---

### `spelunk plumbing embed`

Read lines of text from stdin; emit one embedding vector per line.

```json
{ "index": 0, "embedding": [0.12, -0.34, …] }
```

The vector dimension matches the configured embedding model. Useful for
producing embeddings in batch without invoking the full search pipeline.

---

### `spelunk plumbing graph-edges [--file <path>] [--symbol <name>]`

Emit edges from the code graph. At least one of `--file` or `--symbol`
is required.

```json
{
  "source_file": "src/a.rs",
  "source_name": "foo",
  "target_name": "bar",
  "kind": "calls",
  "line": 42
}
```

Edge kinds: `calls`, `imports`, `extends`, `implements`.

---

### `spelunk plumbing read-memory [--kind <kind>] [--limit <n>] [--id <id>]`

Emit memory entries from the local store.

```json
{
  "id": 7,
  "kind": "decision",
  "title": "Use sqlite-vec for KNN",
  "body": "…",
  "tags": ["storage", "search"],
  "created_at": 1712345678,
  "status": "active"
}
```

`--id` fetches a single entry by id. `--kind` filters by kind. Exit `1` if
no entries match.

---

## Porcelain → Plumbing Composition

Porcelain commands are encouraged (but not required) to compose plumbing
internally. The intended layering:

```
spelunk search "auth flow"
    └── internally equivalent to:
        spelunk plumbing knn "auth flow" --limit 10
        | jq -r '.file + ":" + (.start_line|tostring) + "  " + .name'
```

This means agents that need raw data skip porcelain entirely and call
plumbing directly. Humans use porcelain for readable output.

---

## Agent Usage Pattern

```bash
# Find the top 5 chunks relevant to "token refresh"
spelunk plumbing knn "token refresh" --limit 5 \
  | jq '{file, name, score}'

# Check if a file is stale before re-indexing
spelunk plumbing hash-file src/auth.rs \
  | jq -e '.is_current' > /dev/null || spelunk index .

# Pull all decision-type memory entries
spelunk plumbing read-memory --kind decision \
  | jq -r '"#\(.id)  \(.title)"'
```

---

## Adding a New Plumbing Command

1. Add the args struct in `src/cli/args.rs` under `PlumbingCommand`.
2. Add the handler in `src/cli/cmd/plumbing/<name>.rs`.
3. Wire it in `src/cli/cmd/plumbing/mod.rs` and `src/cli/mod.rs`.
4. Output JSONL, exit 0/1/2, never write to stdout except JSON objects.
5. Add an entry to this document.
