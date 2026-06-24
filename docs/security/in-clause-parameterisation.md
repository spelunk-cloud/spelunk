# Spec: parameterised SQL `IN (...)` clauses in storage

**Status:** approved-for-implementation (straightforward security/robustness fix)
**Area:** `crates/spelunk-core/src/storage/{chunks.rs,graph.rs}`
**Issue:** spelunk#405
**Owner:** Architect → Implementer

---

## 1. Problem

Four query methods assemble their SQL `IN (...)` placeholder list at runtime with
`format!`, then bind the actual values via rusqlite params:

| Function | File | Element type | Value origin |
| --- | --- | --- | --- |
| `chunks_by_ids` | `storage/chunks.rs` | `i64` ids | internal (chunk IDs) |
| `graph_neighbor_chunks` | `storage/graph.rs` | `&str` names | internal symbol names |
| `mention_edges_for_chunks` | `storage/graph.rs` | `i64` ids | internal (chunk IDs) |
| `chunks_mentioning_symbols` | `storage/graph.rs` | `&str` symbols | **AST-extracted, user-file-derived** |

**Current actual risk: low.** Every variant already binds its values through
rusqlite's parameter mechanism (`&dyn ToSql`). Only the `?1,?2,…?N` placeholder
*tokens* — which are generated programmatically and never contain caller data — go
through `format!`. There is no active SQL-injection vector today.

**Why fix it anyway:**

1. **Silent count drift.** The hand-built placeholder list and the separately
   constructed `params` slice must stay length-aligned. A mismatch is a *runtime*
   error (or worse, a silently wrong query in the duplicated-clause case), not a
   compile error.
2. **Duplicated placeholder set.** `graph_neighbor_chunks` references the same
   `?1..?N` set in two `IN` clauses. Correct today, but fragile under edit.
3. **Injection-adjacency.** `chunks_mentioning_symbols` feeds user-file-derived
   strings into a `format!`-built statement. It is safe *only* because the values
   currently flow through bind params. One refactor that moves a symbol into the
   SQL text turns this into real injection. **This is the priority variant.**

## 2. Recommended approach

A single shared helper that produces a placeholder list of exactly `n` slots, used
by all four call sites, with values always bound positionally via rusqlite params.
Standardise on **anonymous `?` placeholders** (not numbered `?N`) so a repeated
clause can re-bind the same logical values without numbering gymnastics.

### 2.1 Helper

Add to a storage-internal helpers module (e.g. `storage/sql.rs` or an existing
`storage/util` location — implementer's choice, keep it `pub(crate)`):

```rust
/// Build a comma-separated list of `n` anonymous bind placeholders: `?,?,?`.
/// Returns an empty string for n == 0 (callers must early-return on empty input).
pub(crate) fn placeholders(n: usize) -> String { /* "?,".repeat(n).pop()-style */ }
```

Anonymous `?` is chosen so that a query needing the same value set in two clauses
just concatenates the value slice twice when binding — no `?N` bookkeeping.

### 2.2 Per-function shape

For each function, the body becomes:

1. Early-return on empty input (already present — keep).
2. **Chunk the input** if it can exceed the bind-parameter limit (see §3).
3. For each chunk, `let ph = placeholders(chunk.len());` and interpolate `ph` into
   the SQL — `ph` is the *only* `format!` argument and contains no caller data.
4. Build `params: Vec<&dyn ToSql>` from the chunk and pass `params.as_slice()`.

| Function | Placeholder slots needed | Param binding |
| --- | --- | --- |
| `chunks_by_ids` | `ids.len()` | bind each `&i64` once |
| `graph_neighbor_chunks` | `names.len()` ×2 clauses | bind the names slice **twice** (clause A then clause B) |
| `mention_edges_for_chunks` | `chunk_ids.len()` | bind each `&i64` once |
| `chunks_mentioning_symbols` | `symbols.len()` | bind each `&str` once — **priority** |

For `graph_neighbor_chunks`: interpolate `placeholders(names.len())` into *both*
`IN (...)` slots, and bind the names slice twice into one params vec
(`names.iter().chain(names.iter())`). This removes the "same `?N` set reused"
trap that the issue calls out.

### 2.3 Symbol-name variant hardening (priority)

`chunks_mentioning_symbols` is the one with user-derived inputs. Beyond the helper:

- Keep all symbol values strictly on the bind-param path. The SQL string must
  contain no symbol bytes — assert this stays true via a unit test that passes a
  symbol containing `') OR 1=1 --` and confirms it returns no rows / is treated
  as a literal value, not SQL.
- Add a `debug_assert_eq!(params.len(), expected_placeholder_count)` guard per
  chunk so a future count drift trips in tests/CI rather than at runtime in prod.

## 3. SQLite bind-parameter limit

SQLite caps bound parameters per statement at `SQLITE_LIMIT_VARIABLE_NUMBER`
(default 999 on older builds, **32766** on SQLite ≥ 3.32). A large `ids` /
`symbols` slice can exceed this and fail at prepare/bind time.

**Requirement:** every one of the four functions must chunk its input list and run
one statement per chunk, merging results. Recommended constant:

```rust
const SQLITE_MAX_BIND: usize = 30_000; // headroom under 32766; ×2 for the two-clause case
```

- `graph_neighbor_chunks` binds its slice **twice per statement**, so its effective
  per-chunk budget is `SQLITE_MAX_BIND / 2`. Size that chunk accordingly.
- Merge semantics: `chunks_by_ids` / `graph_neighbor_chunks` concatenate result
  vecs; the two `HashMap`-returning functions merge maps (extend the per-key
  `Vec`s). De-dup only where the current single-statement query already would
  (e.g. `DISTINCT`); preserve existing behaviour.

## 4. Rejected alternatives

- **Manual quoting/escaping of values into the SQL text** — rejected. Re-introduces
  the exact injection surface we are removing; never embed values in SQL text.
- **`rarray()` / `carray` (sqlite array-bind extension)** — rejected for now. Adds a
  loaded-extension dependency and a `Rc<Vec>` binding dance for marginal benefit
  over the helper; revisit only if chunking ever becomes a hot path.
- **Temp table + JSON1 expansion** — rejected as overkill for these list sizes;
  more moving parts, transaction/lifetime concerns, no current need.
- **Fixed max-N statement with NULL padding** — rejected; wastes the bind budget and
  obscures intent.

## 5. Acceptance criteria

- [ ] All four functions use the shared `placeholders(n)` helper; no `format!("?{}")`
      placeholder construction remains in `storage/chunks.rs` or `storage/graph.rs`.
- [ ] No caller-supplied value (id, name, or symbol) appears in any `format!`
      argument — only the placeholder string does.
- [ ] Each function chunks input at `SQLITE_MAX_BIND` (halved for the two-clause
      `graph_neighbor_chunks`) and merges results with unchanged semantics.
- [ ] `chunks_mentioning_symbols` has a unit test proving a SQL-metacharacter symbol
      is treated as a literal bind value (no injection, no error).
- [ ] `debug_assert_eq!` count guards present on each bind site.
- [ ] Existing callers and behaviour unchanged; full test suite green.
