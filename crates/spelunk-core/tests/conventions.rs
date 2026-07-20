//! Integration and unit tests for convention extraction (#268).
//!
//! All tests use in-memory SQLite so they are hermetic (no LLM, no network).
//! DB tests are annotated `#[serial]` because `sqlite3_auto_extension` is process-global.

mod common;

use serial_test::serial;

use spelunk_core::conventions::{
    ConventionRecord,
    extractor::{ChunkSummary, ConventionExtractor},
    run_extraction,
};
use spelunk_core::storage::ConventionRow;

// ── Helper builders ───────────────────────────────────────────────────────────

fn rust_fn(name: &str, content: &str) -> ChunkSummary {
    ChunkSummary {
        language: "rust".into(),
        node_type: "function".into(),
        name: Some(name.into()),
        content: content.into(),
        file_path: "src/lib.rs".into(),
        has_docstring: content.trim_start().starts_with("///"),
    }
}

fn rust_struct(name: &str) -> ChunkSummary {
    ChunkSummary {
        language: "rust".into(),
        node_type: "struct".into(),
        name: Some(name.into()),
        content: format!("struct {name} {{}}"),
        file_path: "src/lib.rs".into(),
        has_docstring: false,
    }
}

fn ts_fn(name: &str, content: &str) -> ChunkSummary {
    ChunkSummary {
        language: "typescript".into(),
        node_type: "function".into(),
        name: Some(name.into()),
        content: content.into(),
        file_path: "src/index.ts".into(),
        has_docstring: content.trim_start().starts_with("/**"),
    }
}

fn ts_class(name: &str) -> ChunkSummary {
    ChunkSummary {
        language: "typescript".into(),
        node_type: "class".into(),
        name: Some(name.into()),
        content: format!("class {name} {{}}"),
        file_path: "src/models.ts".into(),
        has_docstring: false,
    }
}

fn tsx_fn(name: &str, content: &str) -> ChunkSummary {
    ChunkSummary {
        language: "tsx".into(),
        node_type: "function".into(),
        name: Some(name.into()),
        content: content.into(),
        file_path: "src/App.tsx".into(),
        has_docstring: content.trim_start().starts_with("/**"),
    }
}

fn find_record<'a>(
    records: &'a [ConventionRecord],
    category: &str,
) -> Option<&'a ConventionRecord> {
    records.iter().find(|r| r.category == category)
}

// ── Rust: naming conventions ──────────────────────────────────────────────────

#[test]
fn rust_functions_snake_case() {
    let chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| rust_fn(&format!("do_thing_{i}"), "fn do_thing() {}"))
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, "rust", 0);
    let r = find_record(&records, "naming.functions").expect("naming.functions record");
    assert!(r.confidence >= 0.9, "confidence={}", r.confidence);
    assert!(
        r.description.contains("snake_case"),
        "desc={}",
        r.description
    );
}

#[test]
fn rust_types_pascal_case() {
    let chunks: Vec<ChunkSummary> = ["MyStruct", "AnotherOne", "FooBar", "BazQuux", "HelloWorld"]
        .iter()
        .map(|n| rust_struct(n))
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, "rust", 0);
    let r = find_record(&records, "naming.types").expect("naming.types record");
    assert!(
        r.description.contains("PascalCase"),
        "desc={}",
        r.description
    );
}

#[test]
fn rust_error_handling_anyhow() {
    let content = "use anyhow::Result; fn foo() -> Result<()> { Ok(()) }";
    let chunks: Vec<ChunkSummary> = (0..8).map(|_| rust_fn("foo", content)).collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, "rust", 0);
    let r = find_record(&records, "error_handling").expect("error_handling record");
    assert!(r.description.contains("anyhow"), "desc={}", r.description);
}

#[test]
fn rust_async_runtime_detected() {
    let content = "use tokio::time; async fn handler() { tokio::spawn(async {}); }";
    let chunks: Vec<ChunkSummary> = (0..6).map(|_| rust_fn("handler", content)).collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, "rust", 0);
    let r = find_record(&records, "async").expect("async record");
    assert!(r.description.contains("tokio"), "desc={}", r.description);
}

#[test]
fn rust_testing_cfg_test() {
    let content = "#[cfg(test)] mod tests { #[test] fn it_works() {} }";
    let chunks: Vec<ChunkSummary> = (0..6)
        .map(|i| {
            let mut c = rust_fn(&format!("it_works_{i}"), content);
            c.node_type = "function".into();
            c
        })
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, "rust", 0);
    let r = find_record(&records, "testing").expect("testing record");
    assert!(
        r.description.contains("cfg(test)"),
        "desc={}",
        r.description
    );
}

#[test]
fn rust_doc_coverage_high() {
    let chunks: Vec<ChunkSummary> = (0..8)
        .map(|i| rust_fn(&format!("func_{i}"), "/// Does the thing\nfn func() {}"))
        .chain((0..2).map(|i| rust_fn(&format!("undoc_{i}"), "fn undoc() {}")))
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, "rust", 0);
    let r = find_record(&records, "docs").expect("docs record");
    assert!(r.description.contains("high"), "desc={}", r.description);
}

// ── TypeScript: naming conventions ───────────────────────────────────────────

#[test]
fn ts_functions_camel_case() {
    let chunks: Vec<ChunkSummary> = [
        "getUserById",
        "handleClick",
        "loadConfig",
        "renderPage",
        "fetchData",
        "parseInput",
        "validateForm",
    ]
    .iter()
    .map(|n| ts_fn(n, &format!("function {n}() {{}}")))
    .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::typescript::extract(&refs, "typescript", 0);
    let r = find_record(&records, "naming.functions").expect("naming.functions record");
    assert!(
        r.description.contains("camelCase"),
        "desc={}",
        r.description
    );
}

#[test]
fn ts_types_pascal_case() {
    let chunks: Vec<ChunkSummary> = [
        "UserService",
        "ApiClient",
        "DataModel",
        "EventBus",
        "HttpError",
    ]
    .iter()
    .map(|n| ts_class(n))
    .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::typescript::extract(&refs, "typescript", 0);
    let r = find_record(&records, "naming.types").expect("naming.types record");
    assert!(
        r.description.contains("PascalCase"),
        "desc={}",
        r.description
    );
}

#[test]
fn ts_async_usage_detected() {
    let content = "async function fetchUser() { const data = await fetch('/api/users'); }";
    let chunks: Vec<ChunkSummary> = (0..6)
        .map(|i| ts_fn(&format!("fetchUser{i}"), content))
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::typescript::extract(&refs, "typescript", 0);
    let r = find_record(&records, "async").expect("async record");
    assert!(r.description.contains("async"), "desc={}", r.description);
    assert!(r.confidence > 0.2, "confidence={}", r.confidence);
}

#[test]
fn ts_testing_spec_files() {
    let chunks: Vec<ChunkSummary> = (0..5)
        .map(|i| ChunkSummary {
            language: "typescript".into(),
            node_type: "function".into(),
            name: Some(format!("test_{i}")),
            content: format!("test('does thing {i}', () => {{}})"),
            file_path: "src/components/Button.spec.ts".into(),
            has_docstring: false,
        })
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::typescript::extract(&refs, "typescript", 0);
    let r = find_record(&records, "testing").expect("testing record");
    assert!(r.description.contains("spec.ts"), "desc={}", r.description);
}

// ── ConventionExtractor: multi-language dispatch ──────────────────────────────

#[test]
fn extractor_dispatches_by_language() {
    let rust_chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| rust_fn(&format!("rust_fn_{i}"), "fn rust_fn() {}"))
        .collect();
    let ts_chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| ts_fn(&format!("tsFn{i}"), "function tsFn() {}"))
        .collect();
    let all: Vec<ChunkSummary> = rust_chunks.into_iter().chain(ts_chunks).collect();

    let records = ConventionExtractor::new().extract(&all);
    let rust_records: Vec<_> = records.iter().filter(|r| r.language == "rust").collect();
    let ts_records: Vec<_> = records
        .iter()
        .filter(|r| r.language == "typescript")
        .collect();
    assert!(!rust_records.is_empty(), "should have Rust records");
    assert!(!ts_records.is_empty(), "should have TypeScript records");
}

#[test]
fn extractor_handles_empty_input() {
    let records = ConventionExtractor::new().extract(&[]);
    assert!(records.is_empty());
}

/// Regression: the extractor must emit at most one record per (language,
/// category). Language-specific + always-on generic sets emit overlapping
/// categories (naming.functions, docs), and tsx chunks route through the
/// typescript set (self-labelled "typescript"), so a rust/ts/tsx corpus
/// previously listed those categories two or three times per language.
#[test]
fn extractor_dedups_language_category() {
    let rust_chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| rust_fn(&format!("do_thing_{i}"), "/// doc\nfn do_thing() {}"))
        .collect();
    let ts_chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| {
            ts_fn(
                &format!("getThing{i}"),
                "/** doc */\nfunction getThing() {}",
            )
        })
        .collect();
    // tsx chunks canonicalize onto the typescript label, so without dedup they
    // collide with the ts group's records.
    let tsx_chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| {
            tsx_fn(
                &format!("renderThing{i}"),
                "/** doc */\nfunction renderThing() {}",
            )
        })
        .collect();

    let all: Vec<ChunkSummary> = rust_chunks
        .into_iter()
        .chain(ts_chunks)
        .chain(tsx_chunks)
        .collect();

    let records = ConventionExtractor::new().extract(&all);

    let mut seen = std::collections::HashSet::new();
    for r in &records {
        assert!(
            seen.insert((r.language.clone(), r.category.clone())),
            "duplicate (language, category): ({}, {})",
            r.language,
            r.category
        );
    }

    // The overlapping categories must survive exactly once per language.
    let naming: Vec<_> = records
        .iter()
        .filter(|r| r.language == "typescript" && r.category == "naming.functions")
        .collect();
    assert_eq!(
        naming.len(),
        1,
        "typescript naming.functions must be unique"
    );
    let docs: Vec<_> = records
        .iter()
        .filter(|r| r.language == "rust" && r.category == "docs")
        .collect();
    assert_eq!(docs.len(), 1, "rust docs must be unique");
}

fn find_lang_cat<'a>(
    records: &'a [ConventionRecord],
    language: &str,
    category: &str,
) -> Vec<&'a ConventionRecord> {
    records
        .iter()
        .filter(|r| r.language == language && r.category == category)
        .collect()
}

/// Language-specific wins over generic for overlapping categories.
///
/// Both the rust set and the always-on generic set emit `naming.functions` and
/// `docs` via the same shared helpers, so their description/confidence are
/// identical by construction — the only observable of the win is that the
/// cross-source duplicate is *discarded*, not summed: evidence stays at the
/// single-source count (N), never 2N. A rust-only category (`error_handling`)
/// confirms the language-specific records are the ones flowing through.
#[test]
fn extractor_language_specific_wins_over_generic() {
    const N: u32 = 8;
    let chunks: Vec<ChunkSummary> = (0..N)
        .map(|i| {
            rust_fn(
                &format!("do_thing_{i}"),
                "/// doc\nuse anyhow::Result;\nfn do_thing() -> Result<()> { Ok(()) }",
            )
        })
        .collect();

    let records = ConventionExtractor::new().extract(&chunks);

    let naming = find_lang_cat(&records, "rust", "naming.functions");
    assert_eq!(naming.len(), 1, "naming.functions must be unique");
    assert_eq!(
        naming[0].evidence_count, N,
        "generic duplicate must be discarded, not summed into 2N"
    );

    let docs = find_lang_cat(&records, "rust", "docs");
    assert_eq!(docs.len(), 1, "docs must be unique");
    assert_eq!(
        docs[0].evidence_count, N,
        "generic docs duplicate must be discarded, not summed"
    );

    // error_handling is emitted only by the rust set, so its presence proves
    // the language-specific records survive the merge.
    let eh = find_lang_cat(&records, "rust", "error_handling");
    assert_eq!(eh.len(), 1, "rust error_handling must survive");
    assert!(
        eh[0].description.contains("anyhow"),
        "desc={}",
        eh[0].description
    );
}

/// .ts and .tsx chunks land in one canonical group, so a single
/// `naming.functions` record counts all of them (5 ts + 5 tsx).
#[test]
fn extractor_pools_evidence_across_ts_and_tsx() {
    let ts_chunks: Vec<ChunkSummary> = (0..5)
        .map(|i| ts_fn(&format!("getThing{i}"), "function getThing() {}"))
        .collect();
    let tsx_chunks: Vec<ChunkSummary> = (0..5)
        .map(|i| tsx_fn(&format!("renderThing{i}"), "function renderThing() {}"))
        .collect();
    let all: Vec<ChunkSummary> = ts_chunks.into_iter().chain(tsx_chunks).collect();

    let records = ConventionExtractor::new().extract(&all);

    let naming = find_lang_cat(&records, "typescript", "naming.functions");
    assert_eq!(
        naming.len(),
        1,
        "typescript naming.functions must be unique"
    );
    assert_eq!(
        naming[0].evidence_count, 10,
        "ts (5) + tsx (5) evidence must pool to 10"
    );
    assert!(
        !records.iter().any(|r| r.language == "tsx"),
        "tsx must not survive as its own language group"
    );
}

/// Determinism: `extract` is backed by a BTreeMap keyed on (language, category),
/// so repeated runs on the same input yield the same records in the same,
/// ascending order.
#[test]
fn extractor_extract_is_deterministic() {
    let rust_chunks: Vec<ChunkSummary> = (0..8)
        .map(|i| rust_fn(&format!("do_thing_{i}"), "/// doc\nfn do_thing() {}"))
        .collect();
    let ts_chunks: Vec<ChunkSummary> = (0..8)
        .map(|i| {
            ts_fn(
                &format!("getThing{i}"),
                "/** doc */\nfunction getThing() {}",
            )
        })
        .collect();
    let tsx_chunks: Vec<ChunkSummary> = (0..8)
        .map(|i| tsx_fn(&format!("renderThing{i}"), "function renderThing() {}"))
        .collect();
    let all: Vec<ChunkSummary> = rust_chunks
        .into_iter()
        .chain(ts_chunks)
        .chain(tsx_chunks)
        .collect();

    let keys = |recs: &[ConventionRecord]| -> Vec<(String, String)> {
        recs.iter()
            .map(|r| (r.language.clone(), r.category.clone()))
            .collect()
    };

    let first = keys(&ConventionExtractor::new().extract(&all));
    let second = keys(&ConventionExtractor::new().extract(&all));
    assert_eq!(first, second, "repeated extract must yield identical order");

    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(
        first, sorted,
        "records must be ordered by (language, category)"
    );
}

/// No regression for generic-only languages: go/js/jsx/ruby have no dedicated
/// rule set, so they flow through the generic set alone. Dedup must not drop
/// their legitimate distinct records.
///
/// jsx keeps its own label: only tsx is canonicalized, because only tsx routes
/// through a language-specific set and so could surface under two labels.
/// Folding jsx into javascript would pool their evidence here instead.
#[test]
fn extractor_preserves_generic_only_languages() {
    fn fn_chunk(lang: &str, name: &str) -> ChunkSummary {
        ChunkSummary {
            language: lang.into(),
            node_type: "function".into(),
            name: Some(name.into()),
            content: format!("func {name}() {{}}"),
            file_path: format!("src/main.{lang}"),
            has_docstring: false,
        }
    }

    let mut all: Vec<ChunkSummary> = Vec::new();
    for i in 0..6 {
        all.push(fn_chunk("go", &format!("HandleRequest{i}"))); // PascalCase
        all.push(fn_chunk("javascript", &format!("handleClick{i}"))); // camelCase
        all.push(fn_chunk("jsx", &format!("renderRow{i}"))); // camelCase
        all.push(fn_chunk("ruby", &format!("handle_request_{i}"))); // snake_case
    }

    let records = ConventionExtractor::new().extract(&all);

    for lang in ["go", "javascript", "jsx", "ruby"] {
        let naming = find_lang_cat(&records, lang, "naming.functions");
        assert_eq!(
            naming.len(),
            1,
            "{lang} naming.functions must survive exactly once"
        );
        assert_eq!(naming[0].evidence_count, 6, "{lang} evidence unchanged");
    }
}

/// tsx is typescript plus JSX, so its conventions canonicalize onto the
/// "typescript" label: never emitted under "tsx", and never twice.
#[test]
fn extractor_tsx_conventions_canonicalize_onto_typescript_label_only() {
    let tsx_chunks: Vec<ChunkSummary> = (0..8)
        .map(|i| tsx_fn(&format!("renderThing{i}"), "function renderThing() {}"))
        .collect();

    let records = ConventionExtractor::new().extract(&tsx_chunks);

    let ts = find_lang_cat(&records, "typescript", "naming.functions");
    assert_eq!(
        ts.len(),
        1,
        "tsx naming.functions surfaces once, as typescript"
    );
    assert_eq!(
        ts[0].evidence_count, 8,
        "all tsx evidence lands on that record"
    );

    assert!(
        find_lang_cat(&records, "tsx", "naming.functions").is_empty(),
        "no record may carry the tsx label"
    );
    assert!(
        !records.iter().any(|r| r.language == "tsx"),
        "no category may carry the tsx label"
    );
}

/// Confidence must describe the whole corpus, not the most extreme dialect.
///
/// Splitting .ts from .tsx produced two partial `async` views that the merge
/// collapsed by keeping the *higher* confidence, so a small all-async .tsx group
/// made "async/await is widely used" read 100% when only 9 of 16 functions were
/// async. Pooling the evidence before the rate is computed is what keeps the
/// number honest, and it is what `spelunk context` reports to an agent.
#[test]
fn extractor_async_confidence_pools_evidence_rather_than_taking_max_of_splits() {
    // 10 .ts functions, 3 async.
    let ts_chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| {
            let content = if i < 3 {
                format!("async function loadThing{i}() {{ await fetch('/api'); }}")
            } else {
                format!("function getThing{i}() {{ return 1; }}")
            };
            ts_fn(&format!("thing{i}"), &content)
        })
        .collect();
    // 6 .tsx functions, all async: the lopsided split that used to win outright.
    let tsx_chunks: Vec<ChunkSummary> = (0..6)
        .map(|i| {
            tsx_fn(
                &format!("renderThing{i}"),
                &format!("async function renderThing{i}() {{ await load(); }}"),
            )
        })
        .collect();

    let all: Vec<ChunkSummary> = ts_chunks
        .iter()
        .cloned()
        .chain(tsx_chunks.iter().cloned())
        .collect();
    let records = ConventionExtractor::new().extract(&all);

    let async_recs = find_lang_cat(&records, "typescript", "async");
    assert_eq!(async_recs.len(), 1, "typescript async must be unique");
    let pooled = async_recs[0];

    assert_eq!(pooled.evidence_count, 9, "3 ts + 6 tsx async functions");
    assert!(
        (pooled.confidence - 9.0 / 16.0).abs() < 1e-6,
        "confidence must be the pooled 9/16 rate, got {}",
        pooled.confidence
    );

    // Extracting the .tsx chunks alone reproduces the split whose 100% used to
    // be adopted wholesale.
    let tsx_refs: Vec<&ChunkSummary> = tsx_chunks.iter().collect();
    let split = spelunk_core::conventions::rules::typescript::extract(&tsx_refs, "typescript", 0);
    let split_async = find_record(&split, "async").expect("tsx-only async record");
    assert_eq!(split_async.confidence, 1.0, "tsx-only split is 100% async");
    assert!(
        pooled.confidence < split_async.confidence,
        "pooled confidence ({}) must not inherit the max-of-splits value ({})",
        pooled.confidence,
        split_async.confidence
    );
}

// ── DB round-trip: replace_conventions + list_conventions ─────────────────────

#[test]
#[serial]
fn db_round_trip_replace_and_list() {
    let db = common::open_test_db();

    let rows = vec![
        ConventionRow {
            language: "rust".into(),
            category: "naming.functions".into(),
            description: "Functions use snake_case".into(),
            confidence: 0.9,
            evidence_count: 10,
            extracted_at: 0,
        },
        ConventionRow {
            language: "typescript".into(),
            category: "naming.functions".into(),
            description: "Functions use camelCase".into(),
            confidence: 0.85,
            evidence_count: 7,
            extracted_at: 0,
        },
    ];
    db.replace_conventions(&rows).unwrap();

    let all = db.list_conventions(None).unwrap();
    assert_eq!(all.len(), 2);

    let rust_only = db.list_conventions(Some("rust")).unwrap();
    assert_eq!(rust_only.len(), 1);
    assert_eq!(rust_only[0].description, "Functions use snake_case");
}

#[test]
#[serial]
fn db_replace_is_idempotent() {
    let db = common::open_test_db();
    let row = ConventionRow {
        language: "rust".into(),
        category: "testing".into(),
        description: "Tests in #[cfg(test)] inline modules".into(),
        confidence: 0.8,
        evidence_count: 6,
        extracted_at: 1000,
    };
    db.replace_conventions(std::slice::from_ref(&row)).unwrap();
    db.replace_conventions(std::slice::from_ref(&row)).unwrap();

    let all = db.list_conventions(None).unwrap();
    assert_eq!(all.len(), 1, "replace should delete old records first");
}

#[test]
#[serial]
fn db_list_conventions_empty_when_none_stored() {
    let db = common::open_test_db();
    let all = db.list_conventions(None).unwrap();
    assert!(all.is_empty());
}

// ── End-to-end: run_extraction via DB ─────────────────────────────────────────

#[test]
#[serial]
fn run_extraction_end_to_end() {
    let db = common::open_test_db();

    // Seed 10 Rust snake_case functions.
    let rust_file_id = db
        .upsert_file("src/lib.rs", Some("rust"), "hash1", 0)
        .unwrap();
    for i in 0..10 {
        db.insert_chunk(
            rust_file_id,
            "function",
            Some(&format!("rust_fn_{i}")),
            i,
            i + 5,
            "fn rust_fn() {}",
            None,
            10,
        )
        .unwrap();
    }

    // Seed 10 TypeScript camelCase functions.
    let ts_file_id = db
        .upsert_file("src/index.ts", Some("typescript"), "hash2", 0)
        .unwrap();
    for i in 0..10 {
        db.insert_chunk(
            ts_file_id,
            "function",
            Some(&format!("tsFn{i}")),
            i,
            i + 3,
            "function tsFn() {}",
            None,
            8,
        )
        .unwrap();
    }

    let records = run_extraction(&db).unwrap();
    // After confidence/evidence filtering (>= 0.5, >= 5 evidence), expect results.
    assert!(!records.is_empty(), "extraction should produce records");

    let rust_naming = records
        .iter()
        .find(|r| r.language == "rust" && r.category == "naming.functions");
    let ts_naming = records
        .iter()
        .find(|r| r.language == "typescript" && r.category == "naming.functions");
    assert!(rust_naming.is_some(), "should detect Rust function naming");
    assert!(
        ts_naming.is_some(),
        "should detect TypeScript function naming"
    );
}

// ── list_conventions API wrapper ──────────────────────────────────────────────

#[test]
#[serial]
fn list_conventions_wrapper_converts_correctly() {
    let db = common::open_test_db();
    let rows = vec![ConventionRow {
        language: "rust".into(),
        category: "async".into(),
        description: "Async runtime: tokio".into(),
        confidence: 0.75,
        evidence_count: 8,
        extracted_at: 42,
    }];
    db.replace_conventions(&rows).unwrap();

    let records = spelunk_core::conventions::list_conventions(&db, None).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].language, "rust");
    assert_eq!(records[0].category, "async");
    assert_eq!(records[0].extracted_at, 42);
}

// ── Confidence filtering ──────────────────────────────────────────────────────

#[test]
fn extractor_emits_low_evidence_raw_records() {
    // The extractor emits records regardless of evidence count.
    // run_extraction applies the filter (>= 0.5 confidence AND >= 5 evidence).
    // With 2 evidence points the evidence_count should be < 5.
    let chunks = [
        rust_fn("small_set_a", "fn small_set_a() {}"),
        rust_fn("small_set_b", "fn small_set_b() {}"),
    ];
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let raw = spelunk_core::conventions::rules::rust::extract(&refs, "rust", 0);
    if let Some(r) = raw.iter().find(|r| r.category == "naming.functions") {
        assert!(
            r.evidence_count < 5,
            "evidence_count={} should be below threshold",
            r.evidence_count
        );
    }
}
