# Architecture: Convention Extraction (#268)

Auto-detect project coding conventions from indexed chunks (heuristic, no LLM)
and surface them in `spelunk context`.

---

## Scope

This spec covers OSS v1.0 work only:

- Heuristic extraction from stored chunks (post-index, no LLM, no API keys)
- `conventions` table in the local `spelunk.db`
- Integration into `spelunk context` (new section, both text + JSON)
- Tests for Rust and TypeScript fixtures

**Out of scope** (tracked in cloud-api):
- LLM-driven summarisation of raw convention evidence
- Team-shared conventions via the remote memory server
- A standalone `spelunk conventions` porcelain command

---

## Data Flow

```
spelunk index .
  └─ parse phase       (chunker + ts_walker — unchanged)
  └─ embed phase       (embeddings — unchanged)
  └─ convention phase  ← NEW
       ├─ reads all chunks from spelunk.db for this project
       ├─ runs ConventionExtractor per language
       └─ writes results to conventions table (replaces prior rows)

spelunk context
  └─ reads memory sections (unchanged)
  └─ reads conventions table  ← NEW
  └─ prints/emits combined output
```

---

## Module Map

### New: `crates/spelunk-core/src/conventions/`

```
conventions/
  mod.rs       — pub re-export; ConventionRecord struct; run_extraction()
  extractor.rs — ConventionExtractor: aggregates evidence from chunks
  rules/
    mod.rs     — dispatch to per-language rule sets
    rust.rs    — Rust heuristics (naming, error_handling, async, testing, docs)
    typescript.rs — TypeScript/TSX heuristics
    generic.rs — language-agnostic heuristics (naming, docs)
```

### Modified

| File | Change |
|------|--------|
| `crates/spelunk-core/src/storage/db.rs` | expose `conventions` table methods |
| `crates/spelunk-core/src/storage/mod.rs` | re-export `ConventionRecord`, `insert_conventions`, `list_conventions` |
| `crates/spelunk-cli/src/cli/cmd/index/mod.rs` | call `run_extraction()` after embed phase |
| `crates/spelunk-cli/src/cli/cmd/context.rs` | add conventions section to output |

---

## DB Schema — Migration 019

```sql
-- crates/spelunk-core/migrations/019_conventions.sql

CREATE TABLE IF NOT EXISTS conventions (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    language      TEXT    NOT NULL,
    category      TEXT    NOT NULL,
    description   TEXT    NOT NULL,
    confidence    REAL    NOT NULL DEFAULT 0.0,
    evidence_count INTEGER NOT NULL DEFAULT 0,
    extracted_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_conventions_language
    ON conventions (language);
```

Convention rows are **fully replaced** after each index run:
`DELETE FROM conventions` then re-insert. No partial updates.

---

## Core Types

```rust
// crates/spelunk-core/src/conventions/mod.rs

pub struct ConventionRecord {
    pub language: String,
    pub category: String,      // e.g. "naming.functions", "error_handling", "async"
    pub description: String,   // e.g. "Functions use snake_case"
    pub confidence: f32,       // 0.0–1.0; only emit when >= 0.5
    pub evidence_count: u32,
    pub extracted_at: i64,     // Unix timestamp
}

pub fn run_extraction(db: &Database) -> Result<Vec<ConventionRecord>>;
```

`run_extraction` reads all chunks from `spelunk.db`, dispatches to per-language
extractors, collects `ConventionRecord`s with `confidence >= 0.5`, then writes
them to the `conventions` table (delete-all + insert).

---

## Convention Categories and Heuristics

### Rust

| Category | Heuristic |
|----------|-----------|
| `naming.functions` | Count snake_case vs camelCase in `name` of `kind=function` chunks. Report dominant (≥50%). |
| `naming.types` | Count PascalCase vs other in `name` of `kind=struct\|enum\|trait` chunks. |
| `error_handling` | Regex `\banyhow\b`, `\bthiserror\b`, `\bAppError\b` in chunk content. Report which is dominant. |
| `async` | Count `async fn` occurrences in content. If >20% of function chunks are async, report tokio/async-std (detect from `tokio::`, `async_std::`). |
| `testing` | Count function chunks in files ending `_test.rs` or `tests/` path vs chunks in `#[cfg(test)]` blocks (detect by `#\[cfg\(test\)\]` in content). Report pattern. |
| `docs` | Ratio of chunks with non-null `docstring`. Report coverage level: high (>70%), medium (30–70%), low (<30%). |

### TypeScript / TSX

| Category | Heuristic |
|----------|-----------|
| `naming.functions` | Count camelCase vs snake_case in function/method chunk names. |
| `naming.types` | Count PascalCase in class/interface/type-alias chunk names. |
| `async` | Count `async` keyword occurrences in function chunk content. |
| `testing` | Detect `.test.ts`, `.spec.ts`, `__tests__/` path patterns in indexed file paths. |
| `docs` | Ratio of chunks with non-null `docstring` (JSDoc preceding comment). |

### Generic (all other languages)

| Category | Heuristic |
|----------|-----------|
| `naming.functions` | Case distribution in function chunk names. |
| `docs` | Doc comment coverage ratio. |

---

## Naming Case Detection

```
snake_case:    all lowercase, underscores present, no uppercase
camelCase:     starts lowercase, contains uppercase, no underscores
PascalCase:    starts uppercase, no underscores
SCREAMING:     all uppercase with underscores
unknown:       everything else (single-char names, numeric, etc.)
```

Names with fewer than 3 characters are excluded from case counting (too short to be meaningful).

---

## Confidence Calculation

For binary conventions (present / absent):

```
confidence = evidence_count / total_relevant_chunks_for_language
```

For competing options (e.g. camelCase vs snake_case):

```
confidence = dominant_count / (dominant_count + alternative_count)
```

Only emit a `ConventionRecord` when `confidence >= 0.5`.
Only emit when `evidence_count >= 5` (prevents false positives on near-empty projects).

---

## `spelunk context` Integration

### Text output (existing format extended)

```
── Conventions ──

rust    naming.functions     Functions use snake_case          (0.97, n=142)
rust    error_handling       Error handling via anyhow::Result (0.78, n=89)
rust    async                Async runtime: tokio              (0.91, n=34)
typescript  naming.functions Functions use camelCase           (0.95, n=67)
```

The Conventions section is printed **last**, after all memory sections.
When there are no extracted conventions, the section is silently omitted.

### JSON output — breaking change to `context --format json`

The current output is `[[kind, [notes...]], ...]`.

**New output:**
```json
{
  "memory": [
    ["handoff", [...]],
    ["question", [...]],
    ["decision", [...]],
    ["requirement", [...]]
  ],
  "conventions": [
    {
      "language": "rust",
      "category": "naming.functions",
      "description": "Functions use snake_case",
      "confidence": 0.97,
      "evidence_count": 142,
      "extracted_at": 1748048400
    }
  ]
}
```

This is a **breaking change** to the `context --format json` contract.
Acceptable at v1.0 pre-release; no downstream consumers are pinned to the current format.

---

## Plumbing Command: not implemented

A `spelunk plumbing read-conventions` JSONL dump was scoped alongside this
feature but was never wired up and has since been dropped from v1.0: no
demand signal for an agent-facing conventions dump, and the backing library
(`conventions::list_conventions`, `run_extraction`) already serves `index`
and `context` without it. Revisit only if a real use case shows up; wiring it
would be a small follow-up (module + `PlumbingCommand` arm + dispatch, same
shape as `ls_files`).

---

## Index Phase Integration

In `crates/spelunk-cli/src/cli/cmd/index/mod.rs`, after the embed phase completes:

```rust
// post-embed: extract and store conventions
if let Err(e) = spelunk_core::conventions::run_extraction(&db) {
    eprintln!("warning: convention extraction failed: {e}");
    // non-fatal — index proceeds normally
}
```

Convention extraction failure must **never fail the index**. Log to stderr and continue.

---

## Security Notes (SAMM Design)

- Convention content is derived entirely from the local DB (already-indexed chunks). No new external input surface.
- Regex patterns are compiled once (`OnceLock`) at extraction start, same pattern as `secrets.rs`.
- The `conventions` table is SQLite-local, same access boundary as `chunks`. No new network paths.
- Convention descriptions are templated strings, not user input — no injection surface in `context` output.

---

## Acceptance Criteria Trace

| Criterion | How met |
|-----------|---------|
| AST pass walks chunks, emits candidates (naming, layout, error-handling) | `ConventionExtractor` reads stored chunks; rules cover naming, error handling, async, testing, docs |
| Output integrated into `spelunk context` | New "Conventions" section in both text and JSON output |
| No external dependencies | Pure heuristics, no LLM calls, no network |
| Tests for Rust + TypeScript fixtures | Unit tests in `crates/spelunk-core/src/conventions/` + integration fixture files |

---

## Open Questions (for founder)

None blocking implementation. One deferred decision:

- Should `spelunk conventions refresh` be added as a standalone porcelain command in v1.0, or is triggering via `spelunk index .` sufficient? The issue scopes to `spelunk context` integration only — defer unless founder adds it to acceptance criteria.
