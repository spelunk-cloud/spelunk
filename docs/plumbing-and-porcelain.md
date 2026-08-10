# Plumbing and Porcelain

## What is plumbing vs porcelain?

Git popularised the distinction: *porcelain* commands are polished, human-friendly interfaces (coloured output, progress bars, readable prose), while *plumbing* commands are low-level, composable building blocks designed for scripts and pipelines. spelunk follows the same pattern. Porcelain commands like `spelunk search` and `spelunk memory list` format output for reading in a terminal; plumbing commands under `spelunk plumbing` emit raw JSONL to stdout and are designed to be piped into other processes.

## When to use plumbing

Use plumbing commands when you are:

- **Writing agent scripts** — parse JSONL directly rather than scraping human-readable text.
- **Composing pipelines** — chain plumbing commands with `jq`, `xargs`, or other plumbing commands.
- **Running in CI** — exit codes are unambiguous (see table below); no ANSI codes pollute logs.
- **Building reproducible queries** — the same plumbing invocation always produces the same schema, regardless of terminal width or colour settings.
- **Integrating spelunk output into another tool** — JSONL is trivially parsed in any language.

## When to use porcelain

Use porcelain commands for:

- **Day-to-day developer use:** `spelunk search`, `spelunk explore`, `spelunk memory list` are readable and interactive.
- **Interactive exploration** — `spelunk explore` drives a multi-step agentic loop with formatted summaries.
- **Quick status checks** — `spelunk status`, `spelunk check` give human-readable health reports.

## Exit code convention

| Exit code | Meaning |
|-----------|---------|
| `0` | Command succeeded; one or more results were emitted. |
| `1` | No results found (not an error — treat as empty set). |
| `2` | Hard error — a flag was missing, the DB was not found, or an I/O failure occurred. Diagnostics are written to stderr. |

Scripts should distinguish `1` (empty) from `2` (broken) rather than treating any non-zero exit as fatal.

These codes, and the JSONL field names and types below, are covered by the
[stability contract](stability.md): they are semver-bound, evolve additively
only, and are enforced by tests rather than by convention. `hash-file`, `embed`,
and `publish-notes` cannot return `1`; the contract explains why.

## Output format

All plumbing commands write **one JSON object per line** (JSONL) to **stdout**. Errors and warnings go to **stderr** only — stdout is always machine-parseable. There are no progress bars, no ANSI escape codes, and no trailing commas or array wrappers.

Example: reading five results from `knn` into a shell array:

```bash
mapfile -t results < <(
  echo "auth flow" \
    | spelunk plumbing embed --query \
    | spelunk plumbing knn --limit 5
)
# Each element of $results is a self-contained JSON object.
```

## Composition examples

### Semantic search via embed + knn

Embed a query string and pipe the vector directly into KNN search:

```bash
echo "auth flow" \
  | spelunk plumbing embed --query \
  | spelunk plumbing knn --limit 5 \
  | jq -r '"\(.score | . * 100 | round)%  \(.file_path):\(.start_line)  \(.name // "(anon)")"'
```

`embed --query` prepends the F2LLM instruction prefix expected for queries (`Instruct: …\nQuery: {q}`), producing a JSON object with a `vector` field. `knn` reads that object from stdin and emits one result object per line, sorted by similarity score descending.

### List stale files and re-index only those

```bash
spelunk plumbing ls-files --stale --root . \
  | jq -r '.path' \
  | xargs -I{} spelunk index {}
```

`ls-files --stale` exits `1` if nothing is stale (safe to check `$?` before proceeding). Each emitted object's `.path` field is the project-relative path stored in the index.

## All 9 plumbing commands

Every command below emits JSONL. All but `publish-notes` are read-only: it is the
one that writes and talks to a remote, so treating the namespace as
safe-by-construction for scripting or sandboxing no longer holds.

| Command | Synopsis | Description |
|---------|----------|-------------|
| `cat-chunks` | `spelunk plumbing cat-chunks <file>` | Emit all indexed chunks for a file as JSONL. Exits `1` if the file has no indexed chunks. |
| `ls-files` | `spelunk plumbing ls-files [--prefix <p>] [--stale] [--root <dir>]` | List every indexed file as JSONL. `--stale` restricts output to files whose on-disk hash differs from the stored hash. Exits `1` if no files match. |
| `parse-file` | `spelunk plumbing parse-file <file>` | Parse a file using tree-sitter and emit chunks as JSONL without writing to the index. |
| `hash-file` | `spelunk plumbing hash-file <file>` | Compute the blake3 hash of a file and compare it to the stored hash, reporting whether the index is current for that file. |
| `knn` | `spelunk plumbing knn [--limit N] [--min-score F] [--lang <lang>]` | Read a JSON embedding object from stdin and return the *N* nearest indexed chunks by cosine similarity. Exits `1` if no results pass the filters. |
| `embed` | `spelunk plumbing embed [--query]` | Read lines from stdin and emit one JSONL embedding vector per line. Pass `--query` to apply the query retrieval prefix (use this before piping into `knn`). |
| `graph-edges` | `spelunk plumbing graph-edges --file <f> \| --symbol <s>` | Emit code graph edges (imports, calls, extends) for a file or symbol as JSONL. At least one of `--file` or `--symbol` is required. Exits `1` if no edges found. |
| `read-memory` | `spelunk plumbing read-memory [--kind <k>] [--id <n>] [--limit N]` | Emit memory entries as JSONL. Filter by kind (`decision`, `question`, `note`, etc.) or fetch a single entry by id. |
| `publish-notes` | `spelunk plumbing publish-notes [remote] [--best-effort]` | Publish memory notes (`refs/notes/spelunk`) to `remote` (default `origin`): fetch onto the tracking ref, union-merge with `cat_sort_uniq`, push. Never force-pushes. **Writes and performs network I/O.** `remote` must be a configured remote *name*; a URL is not resolved but reported as `{"published":false,…,"skipped":"no_such_remote"}` at exit `0`. If another process holds the notes lock the publish is skipped rather than run unmerged, reported as `{"published":false,…,"skipped":"lock_unavailable"}` on stdout and as a warning on stderr, at exit `0` with or without `--best-effort`: nothing is lost, and the records publish on your next push. `--best-effort` warns on stderr and exits `0` instead of failing, which is what the pre-push hook uses. Reach for `spelunk hooks install --pre-push` rather than calling this directly. |

---

See also: [Agent Guide](agent-guide.md) for the broader context on using spelunk in agentic workflows.
