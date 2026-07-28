// Tests for the golden-schema checker itself.
//
// Without these, the per-command conformance tests could pass vacuously: a
// checker that never rejects anything makes every golden file green. These pin
// the two halves of the additive-only rule directly on the checker, using
// synthetic rows rather than real command output.

mod schema_contract;
use schema_contract::{
    CommandSchema, Violation, assert_conforms, check_rows, conforming_row, load_golden,
    parse_golden,
};

fn schema(json: &str) -> CommandSchema {
    let doc = format!(r#"{{"commands": {{"probe": {json}}}}}"#);
    parse_golden(&doc).remove("probe").expect("probe schema")
}

fn row(json: &str) -> Vec<serde_json::Value> {
    vec![serde_json::from_str(json).expect("test row is valid JSON")]
}

fn simple_schema() -> CommandSchema {
    schema(r#"{"required": {"path": "string", "count": "integer"}}"#)
}

#[test]
fn conforming_row_yields_no_violations() {
    let violations = check_rows(
        &simple_schema(),
        &row(r#"{"path": "src/lib.rs", "count": 3}"#),
    );
    assert_eq!(violations, vec![]);
}

#[test]
fn adding_an_undeclared_field_is_accepted() {
    let violations = check_rows(
        &simple_schema(),
        &row(r#"{"path": "src/lib.rs", "count": 3, "brand_new_field": "hello"}"#),
    );
    assert_eq!(
        violations,
        vec![],
        "additive evolution is explicitly permitted by the contract"
    );
}

#[test]
fn removing_a_required_field_is_rejected() {
    let violations = check_rows(&simple_schema(), &row(r#"{"path": "src/lib.rs"}"#));
    assert_eq!(
        violations,
        vec![Violation::MissingField {
            line: 1,
            field: "count".to_string(),
        }]
    );
}

#[test]
fn renaming_a_required_field_is_rejected() {
    // A rename is indistinguishable from a removal plus an addition, and the
    // removal half is what breaks every existing consumer.
    let violations = check_rows(
        &simple_schema(),
        &row(r#"{"path": "src/lib.rs", "chunk_count": 3}"#),
    );
    assert_eq!(
        violations,
        vec![Violation::MissingField {
            line: 1,
            field: "count".to_string(),
        }]
    );
}

#[test]
fn retyping_a_required_field_is_rejected() {
    let violations = check_rows(
        &simple_schema(),
        &row(r#"{"path": "src/lib.rs", "count": "3"}"#),
    );
    assert_eq!(
        violations,
        vec![Violation::WrongType {
            line: 1,
            field: "count".to_string(),
            expected: "integer".to_string(),
            actual: "string".to_string(),
        }]
    );
}

#[test]
fn widening_an_integer_field_to_a_float_is_rejected() {
    let violations = check_rows(
        &simple_schema(),
        &row(r#"{"path": "src/lib.rs", "count": 3.5}"#),
    );
    assert_eq!(
        violations,
        vec![Violation::WrongType {
            line: 1,
            field: "count".to_string(),
            expected: "integer".to_string(),
            actual: "number".to_string(),
        }]
    );
}

#[test]
fn a_nullable_field_accepts_both_null_and_its_type() {
    let nullable = schema(r#"{"required": {"name": "string|null"}}"#);
    assert_eq!(check_rows(&nullable, &row(r#"{"name": "parse"}"#)), vec![]);
    assert_eq!(check_rows(&nullable, &row(r#"{"name": null}"#)), vec![]);
    assert_eq!(
        check_rows(&nullable, &row(r#"{"name": 7}"#)),
        vec![Violation::WrongType {
            line: 1,
            field: "name".to_string(),
            expected: "string|null".to_string(),
            actual: "integer".to_string(),
        }]
    );
}

#[test]
fn an_array_field_checks_its_element_type() {
    let arrays = schema(r#"{"required": {"tags": "array<string>"}}"#);
    assert_eq!(check_rows(&arrays, &row(r#"{"tags": []}"#)), vec![]);
    assert_eq!(check_rows(&arrays, &row(r#"{"tags": ["a", "b"]}"#)), vec![]);
    assert_eq!(
        check_rows(&arrays, &row(r#"{"tags": [1, 2]}"#)),
        vec![Violation::WrongType {
            line: 1,
            field: "tags".to_string(),
            expected: "array<string>".to_string(),
            actual: "array<integer>".to_string(),
        }]
    );
}

#[test]
fn an_omitted_optional_field_is_accepted_but_a_mistyped_one_is_not() {
    let with_optional =
        schema(r#"{"required": {"id": "integer"}, "optional": {"source_ref": "string"}}"#);
    assert_eq!(check_rows(&with_optional, &row(r#"{"id": 1}"#)), vec![]);
    assert_eq!(
        check_rows(&with_optional, &row(r#"{"id": 1, "source_ref": "abc123"}"#)),
        vec![]
    );
    assert_eq!(
        check_rows(&with_optional, &row(r#"{"id": 1, "source_ref": 42}"#)),
        vec![Violation::WrongType {
            line: 1,
            field: "source_ref".to_string(),
            expected: "string".to_string(),
            actual: "integer".to_string(),
        }]
    );
}

#[test]
fn every_emitted_line_is_checked_not_just_the_first() {
    let rows = vec![
        serde_json::json!({"path": "a.rs", "count": 1}),
        serde_json::json!({"path": "b.rs"}),
    ];
    assert_eq!(
        check_rows(&simple_schema(), &rows),
        vec![Violation::MissingField {
            line: 2,
            field: "count".to_string(),
        }]
    );
}

#[test]
fn a_non_object_line_is_rejected() {
    let violations = check_rows(&simple_schema(), &row(r#"["not", "an", "object"]"#));
    assert_eq!(violations, vec![Violation::NotAnObject { line: 1 }]);
}

// ── the reporting wrapper, not just the pure checker ─────────────────────────
//
// `check_rows` returns violations; `assert_conforms` is what every per-command
// test actually calls. Testing only the former leaves the wrapper free to
// collect violations and then ignore them, or to accept a command that emitted
// nothing at all, with the whole suite still green.

#[test]
fn assert_conforms_accepts_a_conforming_row() {
    assert_conforms(
        "probe",
        &simple_schema(),
        &row(r#"{"path": "src/lib.rs", "count": 3}"#),
    );
}

#[test]
#[should_panic(expected = "violates the JSONL stability contract")]
fn assert_conforms_panics_on_a_violation_rather_than_collecting_it_silently() {
    assert_conforms("probe", &simple_schema(), &row(r#"{"path": "src/lib.rs"}"#));
}

#[test]
#[should_panic(expected = "emitted no JSONL rows")]
fn assert_conforms_rejects_a_command_that_emitted_nothing() {
    // Zero rows vacuously satisfy every required field, so without this guard a
    // command that stopped emitting anything would still pass its conformance
    // test. That is the failure mode this whole suite exists to prevent.
    assert_conforms("probe", &simple_schema(), &[]);
}

// ── the golden file's own well-formedness ────────────────────────────────────

#[test]
#[should_panic(expected = "guarantees nothing")]
fn a_command_entry_with_no_required_fields_is_rejected() {
    parse_golden(r#"{"commands": {"probe": {"required": {}}}}"#);
}

#[test]
#[should_panic(expected = "both required and optional")]
fn a_field_declared_both_required_and_optional_is_rejected() {
    parse_golden(
        r#"{"commands": {"probe": {"required": {"id": "integer"},
                                   "optional": {"id": "integer"}}}}"#,
    );
}

#[test]
#[should_panic(expected = "unknown type name")]
fn a_misspelled_type_name_is_rejected_rather_than_treated_as_permissive() {
    // A typo that silently degraded to "accept anything" would disable checking
    // for that field while leaving the file looking complete.
    parse_golden(r#"{"commands": {"probe": {"required": {"id": "intger"}}}}"#);
}

#[test]
fn the_shipped_golden_declares_no_field_that_accepts_every_value() {
    // `any` matches anything, so a field declared with it is listed but
    // unguarded. Catching that here keeps the contract from acquiring
    // decorative entries that check nothing.
    for (command, schema) in load_golden() {
        for (field, ty) in schema.required.iter().chain(schema.optional.iter()) {
            assert!(
                ty.counterexample().is_some(),
                "{command}.{field} is declared `{}`, which accepts every value",
                ty.spelling()
            );
        }
    }
}

// ── the same three mutations, applied to every declared surface ──────────────
//
// The per-command conformance tests prove the checker accepts real output. They
// cannot prove it would reject a break, and verifying that by hand on one field
// of one command (as was done for `FileEntry::chunk_count`) says nothing about
// the other eight. These drive removal, rename, and retype across every field
// of every command the golden declares.

#[test]
fn a_row_built_from_the_contract_conforms_for_every_declared_command() {
    for (command, schema) in load_golden() {
        let row = conforming_row(&schema);
        assert_eq!(
            check_rows(&schema, std::slice::from_ref(&row)),
            vec![],
            "{command}: a row built from its own declaration must conform, got {row}"
        );
    }
}

#[test]
fn removing_any_required_field_is_rejected_for_every_declared_command() {
    for (command, schema) in load_golden() {
        let row = conforming_row(&schema);
        for field in schema.required.keys() {
            let mut mutated = row.clone();
            mutated.as_object_mut().unwrap().remove(field);
            assert_eq!(
                check_rows(&schema, &[mutated]),
                vec![Violation::MissingField {
                    line: 1,
                    field: field.clone(),
                }],
                "{command}: dropping `{field}` must be reported as a break"
            );
        }
    }
}

#[test]
fn renaming_any_required_field_is_rejected_for_every_declared_command() {
    for (command, schema) in load_golden() {
        let row = conforming_row(&schema);
        for (field, ty) in &schema.required {
            let mut mutated = row.clone();
            let obj = mutated.as_object_mut().unwrap();
            obj.remove(field);
            obj.insert(format!("{field}_v2"), ty.example());
            assert_eq!(
                check_rows(&schema, &[mutated]),
                vec![Violation::MissingField {
                    line: 1,
                    field: field.clone(),
                }],
                "{command}: renaming `{field}` must be reported as a break"
            );
        }
    }
}

#[test]
fn retyping_any_declared_field_is_rejected_for_every_declared_command() {
    for (command, schema) in load_golden() {
        let row = conforming_row(&schema);
        // Optional fields are included: they are exempt from presence, never
        // from type.
        for (field, ty) in schema.required.iter().chain(schema.optional.iter()) {
            let wrong = ty
                .counterexample()
                .unwrap_or_else(|| panic!("{command}.{field} accepts every value"));
            let mut mutated = row.clone();
            mutated
                .as_object_mut()
                .unwrap()
                .insert(field.clone(), wrong.clone());
            let violations = check_rows(&schema, &[mutated]);
            assert_eq!(
                violations.len(),
                1,
                "{command}: retyping `{field}` to {wrong} must be reported, got {violations:?}"
            );
            assert!(
                matches!(&violations[0], Violation::WrongType { field: f, .. } if f == field),
                "{command}: expected a type violation on `{field}`, got {violations:?}"
            );
        }
    }
}

#[test]
fn adding_a_field_is_accepted_for_every_declared_command() {
    // The other half of the additive-only rule. Asserted across every surface
    // so no single command can quietly acquire a stricter check than the
    // contract promises.
    for (command, schema) in load_golden() {
        let mut row = conforming_row(&schema);
        row.as_object_mut()
            .unwrap()
            .insert("field_added_in_a_later_release".to_string(), 1.into());
        assert_eq!(
            check_rows(&schema, &[row]),
            vec![],
            "{command}: additive evolution must stay allowed"
        );
    }
}
